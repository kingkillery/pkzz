//! Durable host-final reply delivery.
//!
//! The prompt executor owns building and signing one semantic answer. This
//! module owns every later delivery attempt, including restart recovery.
//! Pending records retain the exact signed JSON body required for retry; every
//! terminal record retains only a compact replay-fence tombstone.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use atomic_write_file::AtomicWriteFile;
use nostr::{EventId, PublicKey};
use uuid::Uuid;

#[path = "final_delivery_record.rs"]
mod record;

use record::{
    decode_event_bytes, encode_event_bytes, is_hex_event_id, validate_record,
    HostFinalDeliveryRecord, HostFinalDeliveryState, LoadIssue, OutboxIndex, OutboxScope,
    MAX_LOGICAL_RECEIPT_SUBMISSIONS,
};

use crate::{
    acp::{AcpClient, PromptCompletion},
    observer::{self, ObserverHandle},
    queue::{FinalReplyTarget, FinalReplyTargetError},
    relay::RestClient,
};

const DELIVERY_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_HOST_FINAL_REPLY_BYTES: usize = 64 * 1024;

/// Final delivery's effect on the prompt lifecycle.
///
/// `TerminalFailed` intentionally still lets the prompt task return its normal
/// successful result. The durable record and authenticated observer event are
/// the recovery surface; requeueing the input batch would rerun the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostFinalDeliveryDisposition {
    NotApplicable,
    Accepted,
    TerminalFailed,
}

/// Pair-scoped on-disk store for host-final delivery. One atomically replaced
/// file is retained per signed event ID, including accepted and terminal
/// records, so relay replay cannot silently re-enter ACP after restart.
pub(crate) struct HostFinalDeliveryOutbox {
    directory: PathBuf,
    scope: OutboxScope,
    index: Mutex<OutboxIndex>,
}

#[derive(Debug, Clone)]
enum DeliveryTransition {
    Attempt { attempt: u8 },
    RetryScheduled { attempt: u8 },
    Accepted { attempt: u8 },
    Terminal { reason: &'static str },
}

#[derive(Debug, Clone)]
struct HostFinalDeliveryOutcome {
    disposition: HostFinalDeliveryDisposition,
    event_id: Option<String>,
    channel_id: Option<Uuid>,
    trigger_event_id: Option<String>,
    resumed: bool,
    transitions: Vec<DeliveryTransition>,
}

impl HostFinalDeliveryOutcome {
    fn terminal_without_record(target: Option<&FinalReplyTarget>, reason: &'static str) -> Self {
        Self {
            disposition: HostFinalDeliveryDisposition::TerminalFailed,
            event_id: None,
            channel_id: target.map(|target| target.channel_id),
            trigger_event_id: target.map(|target| target.trigger_event_id.to_hex()),
            resumed: false,
            transitions: vec![DeliveryTransition::Terminal { reason }],
        }
    }

    fn from_record(
        record: &HostFinalDeliveryRecord,
        disposition: HostFinalDeliveryDisposition,
        resumed: bool,
        transitions: Vec<DeliveryTransition>,
    ) -> Self {
        Self {
            disposition,
            event_id: Some(record.event_id.clone()),
            channel_id: record.channel_id.parse().ok(),
            trigger_event_id: Some(record.trigger_event_id.clone()),
            resumed,
            transitions,
        }
    }
}

impl HostFinalDeliveryOutbox {
    /// Create and index an outbox before subscriptions begin. A malformed
    /// existing record is preserved and terminalized where possible; it never
    /// panics or causes a source event to be re-prompted.
    pub(crate) fn open(
        directory: impl Into<PathBuf>,
        relay_url: &str,
        sender_pubkey: &PublicKey,
    ) -> Result<Self, String> {
        let directory = directory.into();
        ensure_restricted_directory(&directory)?;
        let scope = OutboxScope {
            relay_url: relay_url.to_string(),
            sender_pubkey: sender_pubkey.to_hex(),
        };
        let outbox = Self {
            directory,
            scope,
            index: Mutex::new(OutboxIndex::default()),
        };
        outbox.load_existing_records()?;
        Ok(outbox)
    }

    /// Whether a replayed inbound event was already consumed by a durable final
    /// delivery record. The fence includes pending, accepted, and terminal
    /// records intentionally: removing tombstones would reopen the model-rerun
    /// failure after relay replay.
    pub(crate) fn contains_source_event(&self, event_id: &EventId) -> bool {
        self.index
            .lock()
            .map(|index| index.source_event_ids.contains(&event_id.to_hex()))
            .unwrap_or(false)
    }

    /// Persist one signed event before the first delivery attempt. The caller
    /// supplies original JSON bytes and the precomputed source set; this method
    /// validates but never serializes or signs an event.
    pub(crate) fn persist_signed(
        &self,
        event_id: EventId,
        event_bytes: Vec<u8>,
        target: &FinalReplyTarget,
        source_event_ids: &[EventId],
    ) -> Result<(), String> {
        if source_event_ids.is_empty() {
            return Err("host_final_missing_source_events".to_string());
        }
        let record = HostFinalDeliveryRecord {
            version: 1,
            event_id: event_id.to_hex(),
            event_bytes_b64: Some(encode_event_bytes(event_bytes)),
            relay_url: self.scope.relay_url.clone(),
            sender_pubkey: self.scope.sender_pubkey.clone(),
            channel_id: target.channel_id.to_string(),
            trigger_event_id: target.trigger_event_id.to_hex(),
            source_event_ids: source_event_ids.iter().map(EventId::to_hex).collect(),
            logical_attempts: 0,
            state: HostFinalDeliveryState::Pending,
            next_attempt_unix_ms: None,
            diagnostic: None,
        };
        validate_record(&record, &self.scope).map_err(str::to_string)?;
        let mut index = self
            .index
            .lock()
            .map_err(|_| "host_final_outbox_lock_poisoned".to_string())?;
        if index.records.contains_key(&record.event_id) {
            return Err("host_final_duplicate_event_id".to_string());
        }
        self.write_record(&record)?;
        index_record(&mut index, record);
        Ok(())
    }

    /// Send a persisted body until receipt acceptance or a durable terminal
    /// outcome. This is deliberately a two-*logical*-submission budget; the
    /// REST client may perform its documented transport retries underneath each
    /// submission while regenerating only NIP-98 request auth.
    async fn deliver_until_terminal(
        &self,
        event_id: &EventId,
        rest: &RestClient,
        resumed: bool,
    ) -> HostFinalDeliveryOutcome {
        let event_id_hex = event_id.to_hex();
        let mut transitions = Vec::new();
        loop {
            let Some(record) = self.record(&event_id_hex) else {
                transitions.push(DeliveryTransition::Terminal {
                    reason: "outbox_record_missing",
                });
                return HostFinalDeliveryOutcome {
                    disposition: HostFinalDeliveryDisposition::TerminalFailed,
                    event_id: Some(event_id_hex),
                    channel_id: None,
                    trigger_event_id: None,
                    resumed,
                    transitions,
                };
            };

            match record.state {
                HostFinalDeliveryState::Accepted => {
                    return HostFinalDeliveryOutcome::from_record(
                        &record,
                        HostFinalDeliveryDisposition::Accepted,
                        resumed,
                        transitions,
                    );
                }
                HostFinalDeliveryState::TerminalFailed => {
                    return HostFinalDeliveryOutcome::from_record(
                        &record,
                        HostFinalDeliveryDisposition::TerminalFailed,
                        resumed,
                        transitions,
                    );
                }
                HostFinalDeliveryState::Pending | HostFinalDeliveryState::AwaitingRetry => {}
            }

            match validate_record(&record, &self.scope) {
                Err("record_scope_mismatch") => {
                    // Defense in depth: foreign-scope records should never be
                    // indexed, and must not be rewritten terminal here.
                    transitions.push(DeliveryTransition::Terminal {
                        reason: "record_scope_mismatch_skipped",
                    });
                    return HostFinalDeliveryOutcome::from_record(
                        &record,
                        HostFinalDeliveryDisposition::TerminalFailed,
                        resumed,
                        transitions,
                    );
                }
                Err(_) => {
                    let mut terminal = record.clone();
                    terminal.terminalize("record_validation_failed");
                    let reason = if self.replace_record(terminal.clone()).is_ok() {
                        "record_validation_failed"
                    } else {
                        "persist_terminal_failed"
                    };
                    transitions.push(DeliveryTransition::Terminal { reason });
                    return HostFinalDeliveryOutcome::from_record(
                        &terminal,
                        HostFinalDeliveryDisposition::TerminalFailed,
                        resumed,
                        transitions,
                    );
                }
                Ok(()) => {}
            }
            if record.logical_attempts >= MAX_LOGICAL_RECEIPT_SUBMISSIONS {
                let mut terminal = record.clone();
                terminal.terminalize("retry_budget_exhausted");
                let reason = if self.replace_record(terminal.clone()).is_ok() {
                    "retry_budget_exhausted"
                } else {
                    "persist_terminal_failed"
                };
                transitions.push(DeliveryTransition::Terminal { reason });
                return HostFinalDeliveryOutcome::from_record(
                    &terminal,
                    HostFinalDeliveryDisposition::TerminalFailed,
                    resumed,
                    transitions,
                );
            }

            if let Some(next_attempt_unix_ms) = record.next_attempt_unix_ms {
                let now = unix_now_millis();
                if next_attempt_unix_ms > now {
                    let wait = next_attempt_unix_ms.saturating_sub(now);
                    let wait = u64::try_from(wait).unwrap_or(u64::MAX);
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                }
            }

            let mut sending = record.clone();
            sending.logical_attempts += 1;
            sending.state = HostFinalDeliveryState::Pending;
            sending.next_attempt_unix_ms = None;
            sending.diagnostic = None;
            if self.replace_record(sending.clone()).is_err() {
                transitions.push(DeliveryTransition::Terminal {
                    reason: "persist_before_send_failed",
                });
                return HostFinalDeliveryOutcome::from_record(
                    &record,
                    HostFinalDeliveryDisposition::TerminalFailed,
                    resumed,
                    transitions,
                );
            }

            let attempt = sending.logical_attempts;
            transitions.push(DeliveryTransition::Attempt { attempt });
            let bytes = match decode_event_bytes(&sending) {
                Ok(bytes) => bytes,
                Err(_) => {
                    let mut terminal = sending.clone();
                    terminal.terminalize("record_validation_failed");
                    let reason = if self.replace_record(terminal.clone()).is_ok() {
                        "record_validation_failed"
                    } else {
                        "persist_terminal_failed"
                    };
                    transitions.push(DeliveryTransition::Terminal { reason });
                    return HostFinalDeliveryOutcome::from_record(
                        &terminal,
                        HostFinalDeliveryDisposition::TerminalFailed,
                        resumed,
                        transitions,
                    );
                }
            };
            match rest.submit_event_bytes_accepted(&bytes, event_id).await {
                Ok(()) => {
                    let mut accepted = sending;
                    accepted.accept();
                    if self.replace_record(accepted.clone()).is_err() {
                        transitions.push(DeliveryTransition::Terminal {
                            reason: "persist_accepted_failed",
                        });
                        return HostFinalDeliveryOutcome::from_record(
                            &accepted,
                            HostFinalDeliveryDisposition::TerminalFailed,
                            resumed,
                            transitions,
                        );
                    }
                    transitions.push(DeliveryTransition::Accepted { attempt });
                    return HostFinalDeliveryOutcome::from_record(
                        &accepted,
                        HostFinalDeliveryDisposition::Accepted,
                        resumed,
                        transitions,
                    );
                }
                Err(_) if attempt < MAX_LOGICAL_RECEIPT_SUBMISSIONS => {
                    let mut retry = sending;
                    retry.state = HostFinalDeliveryState::AwaitingRetry;
                    retry.next_attempt_unix_ms =
                        Some(unix_now_millis() + DELIVERY_RETRY_DELAY.as_millis());
                    retry.diagnostic = Some("relay_rejected_or_unconfirmed".to_string());
                    if self.replace_record(retry).is_err() {
                        transitions.push(DeliveryTransition::Terminal {
                            reason: "persist_retry_failed",
                        });
                        return HostFinalDeliveryOutcome::from_record(
                            &record,
                            HostFinalDeliveryDisposition::TerminalFailed,
                            resumed,
                            transitions,
                        );
                    }
                    transitions.push(DeliveryTransition::RetryScheduled { attempt });
                }
                Err(_) => {
                    let mut terminal = sending;
                    terminal.terminalize("relay_rejected_or_unconfirmed");
                    if self.replace_record(terminal.clone()).is_err() {
                        transitions.push(DeliveryTransition::Terminal {
                            reason: "persist_terminal_failed",
                        });
                    } else {
                        transitions.push(DeliveryTransition::Terminal {
                            reason: "relay_rejected_or_unconfirmed",
                        });
                    }
                    return HostFinalDeliveryOutcome::from_record(
                        &terminal,
                        HostFinalDeliveryDisposition::TerminalFailed,
                        resumed,
                        transitions,
                    );
                }
            }
        }
    }

    /// Recover pending records after the relay is connected. It operates only
    /// on persisted bytes and does not receive an ACP client or prompt input.
    pub(crate) async fn resume_pending(
        &self,
        rest: &RestClient,
        observer: Option<&ObserverHandle>,
    ) {
        let (issues, pending) = match self.index.lock() {
            Ok(mut index) => (
                std::mem::take(&mut index.load_issues),
                index
                    .records
                    .values()
                    .filter(|record| {
                        matches!(
                            record.state,
                            HostFinalDeliveryState::Pending | HostFinalDeliveryState::AwaitingRetry
                        )
                    })
                    .map(|record| record.event_id.clone())
                    .collect::<Vec<_>>(),
            ),
            Err(_) => (Vec::new(), Vec::new()),
        };

        for issue in issues {
            let outcome = HostFinalDeliveryOutcome {
                disposition: HostFinalDeliveryDisposition::TerminalFailed,
                event_id: issue.event_id,
                channel_id: issue.channel_id,
                trigger_event_id: None,
                resumed: true,
                transitions: vec![DeliveryTransition::Terminal {
                    reason: issue.reason,
                }],
            };
            emit_to_observer(observer, &outcome);
        }

        for event_id_hex in pending {
            let Ok(event_id) = EventId::from_hex(&event_id_hex) else {
                continue;
            };
            if let Some(record) = self.record(&event_id_hex) {
                emit_recovery_started(observer, &record);
            }
            let outcome = self.deliver_until_terminal(&event_id, rest, true).await;
            emit_to_observer(observer, &outcome);
        }
    }

    fn load_existing_records(&self) -> Result<(), String> {
        let entries = fs::read_dir(&self.directory)
            .map_err(|_| "failed_to_read_host_final_outbox".to_string())?;
        for entry in entries {
            let entry = entry.map_err(|_| "failed_to_read_host_final_outbox".to_string())?;
            let path = entry.path();
            let is_json = path.extension().and_then(|extension| extension.to_str()) == Some("json");
            if !is_json {
                continue;
            }
            let is_regular_file = entry
                .file_type()
                .map(|file_type| file_type.is_file() && !file_type.is_symlink())
                .unwrap_or(false);
            if !is_regular_file {
                self.record_load_issue(None, None, "outbox_record_not_regular_file");
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(_) => {
                    self.record_load_issue(
                        filename_event_id(&path),
                        None,
                        "outbox_record_unreadable",
                    );
                    continue;
                }
            };
            let mut record = match serde_json::from_slice::<HostFinalDeliveryRecord>(&bytes) {
                Ok(record) => record,
                Err(_) => {
                    self.record_load_issue(filename_event_id(&path), None, "outbox_record_corrupt");
                    continue;
                }
            };
            let validation = validate_record(&record, &self.scope);
            if validation == Err("record_scope_mismatch") {
                // Shared outbox dirs may contain other identities' pending
                // records. Leave those files untouched and unindexed.
                continue;
            }
            if validation.is_err()
                || record.logical_attempts > MAX_LOGICAL_RECEIPT_SUBMISSIONS
                || (record.logical_attempts == MAX_LOGICAL_RECEIPT_SUBMISSIONS
                    && matches!(
                        record.state,
                        HostFinalDeliveryState::Pending | HostFinalDeliveryState::AwaitingRetry
                    ))
            {
                record.terminalize("record_validation_or_retry_budget_failed");
                if self.write_record(&record).is_err() {
                    self.record_load_issue(
                        Some(record.event_id.clone()),
                        record.channel_id.parse().ok(),
                        "persist_terminal_failed",
                    );
                }
            } else if record.is_terminal() && record.event_bytes_b64.is_some() {
                // Version-1 terminal records from before compaction carried
                // valid signed bytes. Rewrite them once as compact tombstones.
                record.compact_terminal_body();
                if self.write_record(&record).is_err() {
                    self.record_load_issue(
                        Some(record.event_id.clone()),
                        record.channel_id.parse().ok(),
                        "persist_terminal_compaction_failed",
                    );
                }
            }
            if let Ok(mut index) = self.index.lock() {
                index_record(&mut index, record);
            }
        }
        Ok(())
    }

    fn record(&self, event_id: &str) -> Option<HostFinalDeliveryRecord> {
        self.index
            .lock()
            .ok()
            .and_then(|index| index.records.get(event_id).cloned())
    }

    fn replace_record(&self, record: HostFinalDeliveryRecord) -> Result<(), String> {
        self.write_record(&record)?;
        let mut index = self
            .index
            .lock()
            .map_err(|_| "host_final_outbox_lock_poisoned".to_string())?;
        index_record(&mut index, record);
        Ok(())
    }

    fn write_record(&self, record: &HostFinalDeliveryRecord) -> Result<(), String> {
        let payload = serde_json::to_vec(record)
            .map_err(|_| "failed_to_serialize_host_final_record".to_string())?;
        atomic_write_restricted(&self.record_path(&record.event_id)?, &payload)
    }

    fn record_path(&self, event_id: &str) -> Result<PathBuf, String> {
        if !is_hex_event_id(event_id) {
            return Err("invalid_host_final_record_id".to_string());
        }
        Ok(self.directory.join(format!("{event_id}.json")))
    }

    fn record_load_issue(
        &self,
        event_id: Option<String>,
        channel_id: Option<Uuid>,
        reason: &'static str,
    ) {
        if let Ok(mut index) = self.index.lock() {
            index.load_issues.push(LoadIssue {
                event_id,
                channel_id,
                reason,
            });
        }
    }
}

/// Build, sign, serialize exactly once, persist, then drive delivery. All
/// non-accepted exits are terminal observer-visible states; none can enter the
/// prompt retry queue.
pub(crate) async fn finalize_host_final_reply(
    acp: &AcpClient,
    rest: &RestClient,
    outbox: &HostFinalDeliveryOutbox,
    target: Option<&Result<FinalReplyTarget, FinalReplyTargetError>>,
    completion: &PromptCompletion,
    source_event_ids: &[EventId],
) -> HostFinalDeliveryDisposition {
    if !acp.host_final_reply_supported() || !completion.is_publishable_terminal() {
        return HostFinalDeliveryDisposition::NotApplicable;
    }

    let target = match target {
        None => return HostFinalDeliveryDisposition::NotApplicable,
        Some(Ok(target)) => target,
        Some(Err(_)) => {
            let outcome = HostFinalDeliveryOutcome::terminal_without_record(None, "invalid_origin");
            emit_to_acp(acp, &outcome);
            return outcome.disposition;
        }
    };
    let content = match completion.final_reply.as_deref() {
        Some(content) if !content.trim().is_empty() => content,
        _ => {
            let outcome = HostFinalDeliveryOutcome::terminal_without_record(
                Some(target),
                "missing_or_blank_final_reply",
            );
            emit_to_acp(acp, &outcome);
            return outcome.disposition;
        }
    };
    if content.len() > MAX_HOST_FINAL_REPLY_BYTES {
        let outcome = HostFinalDeliveryOutcome::terminal_without_record(
            Some(target),
            "final_reply_too_large",
        );
        emit_to_acp(acp, &outcome);
        return outcome.disposition;
    }

    let mention =
        (target.trigger_author != rest.keys.public_key()).then(|| target.trigger_author.to_hex());
    let mentions: Vec<&str> = mention.as_deref().into_iter().collect();
    let thread_ref = target.thread_ref();
    let builder = match buzz_sdk::build_message(
        target.channel_id,
        content,
        Some(&thread_ref),
        &mentions,
        false,
        &[],
    ) {
        Ok(builder) => builder,
        Err(_) => {
            let outcome =
                HostFinalDeliveryOutcome::terminal_without_record(Some(target), "build_failed");
            emit_to_acp(acp, &outcome);
            return outcome.disposition;
        }
    };
    let event = match rest.sign_event(builder) {
        Ok(event) => event,
        Err(_) => {
            let outcome =
                HostFinalDeliveryOutcome::terminal_without_record(Some(target), "sign_failed");
            emit_to_acp(acp, &outcome);
            return outcome.disposition;
        }
    };
    // This is the only content-event serialization. `persist_signed` validates
    // these bytes but never regenerates them; every transport path uses them.
    let event_bytes = match serde_json::to_vec(&event) {
        Ok(bytes) => bytes,
        Err(_) => {
            let outcome =
                HostFinalDeliveryOutcome::terminal_without_record(Some(target), "serialize_failed");
            emit_to_acp(acp, &outcome);
            return outcome.disposition;
        }
    };
    if outbox
        .persist_signed(event.id, event_bytes, target, source_event_ids)
        .is_err()
    {
        let outcome = HostFinalDeliveryOutcome::terminal_without_record(
            Some(target),
            "outbox_persist_failed",
        );
        emit_to_acp(acp, &outcome);
        return outcome.disposition;
    }

    let outcome = outbox.deliver_until_terminal(&event.id, rest, false).await;
    emit_to_acp(acp, &outcome);
    outcome.disposition
}

fn emit_to_acp(acp: &AcpClient, outcome: &HostFinalDeliveryOutcome) {
    emit_outcome(|kind, payload| acp.observe(kind, payload), outcome);
}

fn emit_to_observer(observer: Option<&ObserverHandle>, outcome: &HostFinalDeliveryOutcome) {
    let Some(observer) = observer else {
        return;
    };
    let context = observer::context_for(outcome.channel_id, None, None);
    emit_outcome(
        |kind, payload| observer.emit(kind, None, &context, payload),
        outcome,
    );
}

fn emit_recovery_started(observer: Option<&ObserverHandle>, record: &HostFinalDeliveryRecord) {
    let Some(observer) = observer else {
        return;
    };
    let channel_id = record.channel_id.parse().ok();
    let context = observer::context_for(channel_id, None, None);
    observer.emit(
        "reply_delivery_recovery_resumed",
        None,
        &context,
        serde_json::json!({
            "eventId": record.event_id,
            "triggerEventId": record.trigger_event_id,
            "channelId": record.channel_id,
            "resumed": true,
        }),
    );
}

fn emit_outcome(
    mut emit: impl FnMut(&'static str, serde_json::Value),
    outcome: &HostFinalDeliveryOutcome,
) {
    for transition in &outcome.transitions {
        let common = serde_json::json!({
            "eventId": outcome.event_id,
            "triggerEventId": outcome.trigger_event_id,
            "channelId": outcome.channel_id.map(|channel_id| channel_id.to_string()),
            "resumed": outcome.resumed,
        });
        match transition {
            DeliveryTransition::Attempt { attempt } => {
                tracing::info!(
                    event_id = ?outcome.event_id,
                    attempt,
                    resumed = outcome.resumed,
                    "host final delivery attempt"
                );
                let mut payload = common;
                payload["attempt"] = serde_json::json!(attempt);
                emit("reply_delivery_attempt", payload);
            }
            DeliveryTransition::RetryScheduled { attempt } => {
                tracing::warn!(
                    event_id = ?outcome.event_id,
                    attempt,
                    resumed = outcome.resumed,
                    "host final delivery receipt unconfirmed; retry scheduled"
                );
                let mut failure = common.clone();
                failure["attempt"] = serde_json::json!(attempt);
                failure["reason"] = serde_json::json!("relay_rejected_or_unconfirmed");
                failure["willRetry"] = serde_json::json!(true);
                emit("reply_delivery_failed", failure);

                let mut retry = common;
                retry["attempt"] = serde_json::json!(attempt);
                retry["delayMs"] = serde_json::json!(DELIVERY_RETRY_DELAY.as_millis());
                emit("reply_delivery_retry_scheduled", retry);
            }
            DeliveryTransition::Accepted { attempt } => {
                tracing::info!(
                    event_id = ?outcome.event_id,
                    attempt,
                    resumed = outcome.resumed,
                    "host final delivery accepted"
                );
                let mut payload = common;
                payload["attempt"] = serde_json::json!(attempt);
                emit("reply_delivery_accepted", payload);
            }
            DeliveryTransition::Terminal { reason } => {
                tracing::error!(
                    event_id = ?outcome.event_id,
                    reason,
                    resumed = outcome.resumed,
                    "host final delivery terminal"
                );
                let mut payload = common;
                payload["reason"] = serde_json::json!(reason);
                emit("reply_delivery_terminal", payload);
            }
        }
    }
}

fn index_record(index: &mut OutboxIndex, record: HostFinalDeliveryRecord) {
    for source_event_id in &record.source_event_ids {
        index.source_event_ids.insert(source_event_id.clone());
    }
    index.records.insert(record.event_id.clone(), record);
}

fn ensure_restricted_directory(directory: &Path) -> Result<(), String> {
    fs::create_dir_all(directory).map_err(|_| "failed_to_create_host_final_outbox".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .map_err(|_| "failed_to_restrict_host_final_outbox".to_string())?;
    }
    Ok(())
}

fn atomic_write_restricted(path: &Path, payload: &[u8]) -> Result<(), String> {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut file = AtomicWriteFile::open(&resolved)
        .map_err(|_| "failed_to_open_host_final_record".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| "failed_to_restrict_host_final_record".to_string())?;
    }
    file.write_all(payload)
        .map_err(|_| "failed_to_write_host_final_record".to_string())?;
    file.commit()
        .map_err(|_| "failed_to_commit_host_final_record".to_string())
}

fn filename_event_id(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    is_hex_event_id(stem).then(|| stem.to_string())
}

fn unix_now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
#[path = "final_delivery_tests.rs"]
mod tests;
