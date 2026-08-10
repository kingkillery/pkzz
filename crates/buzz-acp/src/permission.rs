//! Process-local owner-permission rendezvous.
//!
//! Pending approvals are deliberately ephemeral. They are bound to the exact
//! managed-agent identity, configured relay, ACP session, and a fresh UUID,
//! and disappear on the first decision, cancellation, timeout, disable, or
//! waiter drop.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use tokio::sync::oneshot;
use tokio::time::Instant;
use uuid::Uuid;

use crate::observer::ObserverContext;

/// Maximum time an owner may take to decide a permission request.
pub const PERMISSION_DECISION_TIMEOUT: Duration = Duration::from_secs(120);

/// Process-wide identity scope of the permission bridge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionScope {
    pub agent_pubkey: String,
    pub relay_url: String,
}

impl PermissionScope {
    pub fn new(agent_pubkey: &str, relay_url: &str) -> Result<Self, &'static str> {
        if agent_pubkey.len() != 64
            || !agent_pubkey.bytes().all(|byte| byte.is_ascii_hexdigit())
            || agent_pubkey.bytes().any(|byte| byte.is_ascii_uppercase())
        {
            return Err("agent pubkey must be exactly 64 lowercase hex characters");
        }
        if relay_url.is_empty() {
            return Err("relay URL must not be empty");
        }
        Ok(Self {
            agent_pubkey: agent_pubkey.to_owned(),
            relay_url: relay_url.to_owned(),
        })
    }
}

/// Complete, replay-resistant binding for one live ACP permission request.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionBinding {
    pub agent_pubkey: String,
    pub relay_url: String,
    pub session_id: String,
    pub request_id: Uuid,
}

/// The only decisions accepted from the owner control channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerPermissionDecision {
    ApproveOnce,
    Reject,
}

/// Result of atomically dispatching an owner decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDispatchStatus {
    Delivered,
    NotPending,
}

/// Terminal condition observed by the ACP read loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionWaitOutcome {
    Decision(OwnerPermissionDecision),
    Expired,
    Cancelled,
    Unavailable,
}

struct PendingPermission {
    sender: oneshot::Sender<PermissionWaitOutcome>,
    deadline: Instant,
    context: ObserverContext,
}

#[derive(Default)]
struct BrokerState {
    scope: Option<PermissionScope>,
    pending: HashMap<PermissionBinding, PendingPermission>,
}

struct BrokerInner {
    state: Mutex<BrokerState>,
}

impl Default for BrokerInner {
    fn default() -> Self {
        Self {
            state: Mutex::new(BrokerState::default()),
        }
    }
}

/// Cloneable, process-local one-shot permission broker.
#[derive(Clone, Default)]
pub struct PermissionBroker {
    inner: Arc<BrokerInner>,
}

impl PermissionBroker {
    /// Enable the broker for one exact managed-agent/relay scope.
    ///
    /// Switching scope first drains old requests as unavailable. Re-enabling
    /// the identical scope is idempotent and preserves live requests.
    pub fn enable(&self, scope: PermissionScope) {
        let mut state = self.lock_state();
        if state.scope.as_ref() == Some(&scope) {
            return;
        }
        Self::drain_locked(&mut state, PermissionWaitOutcome::Unavailable);
        state.scope = Some(scope);
    }

    /// Disable the broker and wake all pending waiters into the deny path.
    pub fn disable(&self) {
        let mut state = self.lock_state();
        state.scope = None;
        Self::drain_locked(&mut state, PermissionWaitOutcome::Unavailable);
    }

    /// Register one request. Returns `None` when the bridge is disabled or the
    /// supplied session does not exactly match the captured observer context.
    pub fn register(
        &self,
        session_id: &str,
        context: ObserverContext,
        deadline: Instant,
    ) -> Option<PermissionWaiter> {
        if session_id.is_empty() || context.session_id.as_deref() != Some(session_id) {
            return None;
        }

        let mut state = self.lock_state();
        let scope = state.scope.clone()?;
        let (sender, receiver) = oneshot::channel();
        let binding = loop {
            let candidate = PermissionBinding {
                agent_pubkey: scope.agent_pubkey.clone(),
                relay_url: scope.relay_url.clone(),
                session_id: session_id.to_owned(),
                request_id: Uuid::new_v4(),
            };
            if !state.pending.contains_key(&candidate) {
                break candidate;
            }
        };
        let display_remaining = deadline.saturating_duration_since(Instant::now());
        let expires_at = chrono::Utc::now()
            + chrono::Duration::from_std(display_remaining).unwrap_or(chrono::Duration::MAX);
        state.pending.insert(
            binding.clone(),
            PendingPermission {
                sender,
                deadline,
                context: context.clone(),
            },
        );
        Some(PermissionWaiter {
            broker: Arc::downgrade(&self.inner),
            binding,
            deadline,
            expires_at: expires_at.to_rfc3339(),
            receiver: Some(receiver),
        })
    }

    /// Atomically consume a matching pending request and deliver its decision.
    /// Every mismatch and replay deliberately returns the same non-oracular
    /// `NotPending` status.
    pub fn resolve(
        &self,
        binding: &PermissionBinding,
        decision: OwnerPermissionDecision,
    ) -> PermissionDispatchStatus {
        let mut state = self.lock_state();
        let Some(pending) = state.pending.remove(binding) else {
            return PermissionDispatchStatus::NotPending;
        };
        if Instant::now() >= pending.deadline {
            let _ = pending.sender.send(PermissionWaitOutcome::Expired);
            return PermissionDispatchStatus::NotPending;
        }
        if pending
            .sender
            .send(PermissionWaitOutcome::Decision(decision))
            .is_ok()
        {
            PermissionDispatchStatus::Delivered
        } else {
            PermissionDispatchStatus::NotPending
        }
    }

    /// Cancel one exact request, if still pending.
    pub fn cancel(&self, binding: &PermissionBinding) -> PermissionDispatchStatus {
        let mut state = self.lock_state();
        let Some(pending) = state.pending.remove(binding) else {
            return PermissionDispatchStatus::NotPending;
        };
        if pending
            .sender
            .send(PermissionWaitOutcome::Cancelled)
            .is_ok()
        {
            PermissionDispatchStatus::Delivered
        } else {
            PermissionDispatchStatus::NotPending
        }
    }

    /// Cancel only requests associated with the exact Pkzz channel.
    pub fn cancel_channel(&self, channel_id: &str) -> usize {
        let mut state = self.lock_state();
        let bindings: Vec<_> = state
            .pending
            .iter()
            .filter(|(_, pending)| pending.context.channel_id.as_deref() == Some(channel_id))
            .map(|(binding, _)| binding.clone())
            .collect();
        let count = bindings.len();
        for binding in bindings {
            if let Some(pending) = state.pending.remove(&binding) {
                let _ = pending.sender.send(PermissionWaitOutcome::Cancelled);
            }
        }
        count
    }

    fn remove(&self, binding: &PermissionBinding) {
        self.lock_state().pending.remove(binding);
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, BrokerState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn drain_locked(state: &mut BrokerState, outcome: PermissionWaitOutcome) {
        for (_, pending) in state.pending.drain() {
            let _ = pending.sender.send(outcome);
        }
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.lock_state().pending.len()
    }
}

/// RAII registration returned to the ACP read loop.
///
/// Dropping the future that owns this value unregisters the request before a
/// subsequent replay can be admitted.
pub struct PermissionWaiter {
    broker: Weak<BrokerInner>,
    binding: PermissionBinding,
    deadline: Instant,
    expires_at: String,
    receiver: Option<oneshot::Receiver<PermissionWaitOutcome>>,
}

impl PermissionWaiter {
    pub fn binding(&self) -> &PermissionBinding {
        &self.binding
    }

    pub fn expires_at(&self) -> &str {
        &self.expires_at
    }

    pub async fn wait(mut self) -> PermissionWaitOutcome {
        let mut receiver = self.receiver.take().expect("permission waiter polled once");
        tokio::select! {
            biased;
            result = &mut receiver => {
                result.unwrap_or(PermissionWaitOutcome::Unavailable)
            }
            _ = tokio::time::sleep_until(self.deadline) => {
                if let Some(inner) = self.broker.upgrade() {
                    PermissionBroker { inner }.remove(&self.binding);
                }
                PermissionWaitOutcome::Expired
            }
        }
    }
}

impl Drop for PermissionWaiter {
    fn drop(&mut self) {
        if let Some(inner) = self.broker.upgrade() {
            PermissionBroker { inner }.remove(&self.binding);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> PermissionScope {
        PermissionScope::new(&"a".repeat(64), "wss://relay.example").unwrap()
    }

    fn context(session_id: &str, channel_id: Option<&str>) -> ObserverContext {
        ObserverContext {
            channel_id: channel_id.map(str::to_owned),
            session_id: Some(session_id.to_owned()),
            turn_id: Some("turn-1".into()),
            started_at: Some("2026-08-06T00:00:00Z".into()),
        }
    }

    fn register(broker: &PermissionBroker, session_id: &str) -> PermissionWaiter {
        broker
            .register(
                session_id,
                context(session_id, Some("channel-a")),
                Instant::now() + PERMISSION_DECISION_TIMEOUT,
            )
            .expect("enabled broker")
    }

    #[tokio::test]
    async fn decision_requires_exact_agent_relay_session_and_request() {
        let broker = PermissionBroker::default();
        broker.enable(scope());
        let waiter = register(&broker, "session-a");
        let binding = waiter.binding().clone();

        let mut mutations = Vec::new();
        let mut wrong_agent = binding.clone();
        wrong_agent.agent_pubkey = "b".repeat(64);
        mutations.push(wrong_agent);
        let mut wrong_relay = binding.clone();
        wrong_relay.relay_url.push_str("/other");
        mutations.push(wrong_relay);
        let mut wrong_session = binding.clone();
        wrong_session.session_id = "session-b".into();
        mutations.push(wrong_session);
        let mut wrong_request = binding.clone();
        wrong_request.request_id = Uuid::new_v4();
        mutations.push(wrong_request);

        for candidate in mutations {
            assert_eq!(
                broker.resolve(&candidate, OwnerPermissionDecision::ApproveOnce),
                PermissionDispatchStatus::NotPending
            );
        }
        assert_eq!(broker.pending_count(), 1);
        assert_eq!(
            broker.resolve(&binding, OwnerPermissionDecision::ApproveOnce),
            PermissionDispatchStatus::Delivered
        );
        assert_eq!(
            waiter.wait().await,
            PermissionWaitOutcome::Decision(OwnerPermissionDecision::ApproveOnce)
        );
    }

    #[tokio::test]
    async fn first_valid_decision_wins_and_replay_is_not_pending() {
        let broker = PermissionBroker::default();
        broker.enable(scope());
        let waiter = register(&broker, "session-a");
        let binding = waiter.binding().clone();

        assert_eq!(
            broker.resolve(&binding, OwnerPermissionDecision::Reject),
            PermissionDispatchStatus::Delivered
        );
        assert_eq!(
            broker.resolve(&binding, OwnerPermissionDecision::ApproveOnce),
            PermissionDispatchStatus::NotPending
        );
        assert_eq!(
            waiter.wait().await,
            PermissionWaitOutcome::Decision(OwnerPermissionDecision::Reject)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn waiter_timeout_and_drop_remove_registration() {
        let broker = PermissionBroker::default();
        broker.enable(scope());
        let waiter = register(&broker, "session-a");
        let binding = waiter.binding().clone();
        let wait_task = tokio::spawn(async move { waiter.wait().await });
        tokio::time::advance(PERMISSION_DECISION_TIMEOUT).await;
        assert_eq!(wait_task.await.unwrap(), PermissionWaitOutcome::Expired);
        assert_eq!(broker.pending_count(), 0);
        assert_eq!(
            broker.resolve(&binding, OwnerPermissionDecision::ApproveOnce),
            PermissionDispatchStatus::NotPending
        );

        let dropped = register(&broker, "session-a");
        let dropped_binding = dropped.binding().clone();
        drop(dropped);
        assert_eq!(broker.pending_count(), 0);
        assert_eq!(
            broker.resolve(&dropped_binding, OwnerPermissionDecision::Reject),
            PermissionDispatchStatus::NotPending
        );
    }

    #[tokio::test]
    async fn disable_drains_all_pending_as_denials() {
        let broker = PermissionBroker::default();
        broker.enable(scope());
        let first = register(&broker, "session-a");
        let second = register(&broker, "session-b");
        broker.disable();
        assert_eq!(first.wait().await, PermissionWaitOutcome::Unavailable);
        assert_eq!(second.wait().await, PermissionWaitOutcome::Unavailable);
        assert_eq!(broker.pending_count(), 0);
        assert!(broker
            .register(
                "session-c",
                context("session-c", Some("channel-c")),
                Instant::now() + PERMISSION_DECISION_TIMEOUT,
            )
            .is_none());
    }

    #[test]
    fn request_ids_are_fresh_across_rpc_id_and_session_reuse() {
        let broker = PermissionBroker::default();
        broker.enable(scope());
        let first = register(&broker, "session-a");
        let second = register(&broker, "session-a");
        assert_ne!(first.binding().request_id, second.binding().request_id);
    }

    #[tokio::test]
    async fn channel_cancel_is_scoped_and_preserves_heartbeat_requests() {
        let broker = PermissionBroker::default();
        broker.enable(scope());
        let first = register(&broker, "session-a");
        let other = broker
            .register(
                "session-b",
                context("session-b", Some("channel-b")),
                Instant::now() + PERMISSION_DECISION_TIMEOUT,
            )
            .unwrap();
        let heartbeat = broker
            .register(
                "session-heartbeat",
                context("session-heartbeat", None),
                Instant::now() + PERMISSION_DECISION_TIMEOUT,
            )
            .unwrap();

        assert_eq!(broker.cancel_channel("channel-a"), 1);
        assert_eq!(first.wait().await, PermissionWaitOutcome::Cancelled);
        assert_eq!(broker.pending_count(), 2);

        let other_binding = other.binding().clone();
        assert_eq!(
            broker.resolve(&other_binding, OwnerPermissionDecision::Reject),
            PermissionDispatchStatus::Delivered
        );
        assert_eq!(
            other.wait().await,
            PermissionWaitOutcome::Decision(OwnerPermissionDecision::Reject)
        );
        drop(heartbeat);
    }

    #[test]
    fn scope_validation_is_exact_and_case_sensitive() {
        assert!(PermissionScope::new(&"a".repeat(64), "wss://relay.example").is_ok());
        assert!(PermissionScope::new(&"A".repeat(64), "wss://relay.example").is_err());
        assert!(PermissionScope::new(&"a".repeat(63), "wss://relay.example").is_err());
        assert!(PermissionScope::new(&"g".repeat(64), "wss://relay.example").is_err());
        assert!(PermissionScope::new(&"a".repeat(64), "").is_err());
    }
}
