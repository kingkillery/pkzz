//! Thread participation tracking for `engagement = "thread"` rules.
//!
//! Answers one question deterministically: *is this inbound event a reply
//! into a conversation the agent is already part of?* — without any model
//! call. "Part of" means the agent has authored an event in the thread
//! (posted the root, or replied anywhere inside it).
//!
//! ## Data flow
//!
//! Population happens at the main loop's self-event branch: thread-engaged
//! channels use channel-wide delivery, so every event the agent publishes
//! (via `buzz-cli` inside the agent subprocess, or via harness-side
//! failure/final delivery — all the same keypair) arrives back on the relay
//! stream and is recorded before being dropped as self-authored. Restart
//! amnesia is bounded by [`crate::lib`]-side rehydration: a startup REST
//! query over the agent's recent authored events replays them through the
//! same recording path.
//!
//! ## Memory bounds
//!
//! Both sets use two-generation rotation (same tradeoff as the relay-side
//! event dedup): at least `limit/2` and at most `limit` entries are
//! remembered, oldest half forgotten on rotation. Forgetting participation
//! degrades to "agent needs an explicit @mention in that thread again" —
//! safe by construction, never over-engaging.

use std::collections::HashSet;

use uuid::Uuid;

/// Maximum tracked self-authored event ids before generation rotation.
const OWN_EVENT_LIMIT: usize = 4_096;
/// Maximum tracked participated thread roots before generation rotation.
const THREAD_LIMIT: usize = 1_024;

/// Two-generation bounded set of strings (event ids / thread-root keys).
///
/// `insert` returns `true` when the value was not already present.
#[derive(Debug, Default)]
struct TwoGenSet {
    current: HashSet<String>,
    previous: HashSet<String>,
    half_limit: usize,
}

impl TwoGenSet {
    fn new(limit: usize) -> Self {
        Self {
            current: HashSet::new(),
            previous: HashSet::new(),
            half_limit: (limit / 2).max(1),
        }
    }

    fn insert(&mut self, value: String) -> bool {
        if self.previous.contains(&value) || self.current.contains(&value) {
            return false;
        }
        if self.current.len() >= self.half_limit {
            self.previous = std::mem::take(&mut self.current);
        }
        self.current.insert(value)
    }

    fn contains(&self, value: &str) -> bool {
        self.current.contains(value) || self.previous.contains(value)
    }
}

/// Ephemeral Nostr kind range lower bound (NIP-01: 20000..=29999).
const EPHEMERAL_KIND_START: u16 = 20_000;
/// Ephemeral Nostr kind range upper bound.
const EPHEMERAL_KIND_END: u16 = 29_999;
/// NIP-09 deletion kind — never heads a conversation.
const KIND_DELETION: u16 = 5;
/// NIP-25 reaction kind — never heads a conversation.
const KIND_REACTION: u16 = 7;

/// Tracks the agent's own authored events and the thread roots it has
/// participated in, per channel.
#[derive(Debug)]
pub struct ParticipationTracker {
    /// Event ids authored by the agent (hex), channel-agnostic.
    ///
    /// A reply referencing one of these ids is a reply *to the agent*.
    own_events: TwoGenSet,
    /// `"<channel_uuid>:<root_id_hex>"` keys for threads the agent posted in.
    threads: TwoGenSet,
}

impl Default for ParticipationTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ParticipationTracker {
    pub fn new() -> Self {
        Self {
            own_events: TwoGenSet::new(OWN_EVENT_LIMIT),
            threads: TwoGenSet::new(THREAD_LIMIT),
        }
    }

    fn thread_key(channel_id: Uuid, root_hex: &str) -> String {
        format!("{channel_id}:{root_hex}")
    }

    /// Record a self-authored event: its id becomes reply-addressable, and
    /// any thread it references becomes a participated thread.
    ///
    /// Reactions, deletions, and ephemeral kinds are skipped — they never
    /// carry conversation and would only churn the bounded sets.
    pub fn record_self_event(&mut self, channel_id: Uuid, event: &nostr::Event) {
        let kind = event.kind.as_u16();
        if kind == KIND_DELETION
            || kind == KIND_REACTION
            || (EPHEMERAL_KIND_START..=EPHEMERAL_KIND_END).contains(&kind)
        {
            return;
        }

        self.own_events.insert(event.id.to_hex());

        // Any e-tag reference marks participation in that thread. Positional
        // NIP-10 shape: ["e", <id>, <relay?>, <marker?>]. The root marker is
        // preferred, but a bare/reply reference still proves participation —
        // record every referenced id so replies-to-siblings inside the same
        // thread resolve without needing the true root.
        for id in referenced_event_ids(event) {
            self.threads.insert(Self::thread_key(channel_id, &id));
        }
    }

    /// If `event` is a reply into a thread the agent participates in, return
    /// the id that proved it (for guardrail keying). `None` = not engaged.
    ///
    /// Engagement holds when any referenced event id is either an event the
    /// agent authored, or a recorded participated-thread reference in this
    /// channel.
    pub fn thread_engagement_root(&self, channel_id: Uuid, event: &nostr::Event) -> Option<String> {
        referenced_event_ids(event).find(|id| {
            self.own_events.contains(id) || self.threads.contains(&Self::thread_key(channel_id, id))
        })
    }
}

/// All event ids referenced by `e` tags, first-position value.
fn referenced_event_ids(event: &nostr::Event) -> impl Iterator<Item = String> + '_ {
    event.tags.iter().filter_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(|k| k.as_str()) == Some("e") {
            s.get(1).map(|v| v.to_string())
        } else {
            None
        }
    })
}

/// Guardrails for non-mention (thread-engaged) turns.
///
/// Two independent brakes, both bypassed by explicit @mentions and both
/// reset by the owner speaking in the thread:
///
/// - **Chain cap**: at most `max_agent_chain` consecutive thread-engaged
///   turns per thread without an owner event or explicit mention. Stops
///   two agents ping-ponging each other forever.
/// - **Cooldown**: minimum spacing between thread-engaged turns per
///   channel. Flood brake for bursty threads.
#[derive(Debug)]
pub struct EngagementGuard {
    max_agent_chain: u32,
    cooldown: std::time::Duration,
    /// `"<channel>:<root>"` → consecutive thread-engaged turn count.
    chains: std::collections::HashMap<String, u32>,
    /// Per-channel instant of the last allowed thread-engaged turn.
    last_engaged: std::collections::HashMap<Uuid, std::time::Instant>,
}

/// Why a thread-engaged turn was suppressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressReason {
    /// The per-thread consecutive agent-turn cap was reached.
    ChainCap,
    /// The per-channel cooldown window has not elapsed.
    Cooldown,
}

impl SuppressReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SuppressReason::ChainCap => "chain_cap",
            SuppressReason::Cooldown => "cooldown",
        }
    }
}

/// Bound on tracked chains; oldest-generation eviction is not required here
/// because entries are removed on reset and the key space is small, but a
/// hard cap keeps a pathological channel from growing the map unbounded.
const CHAIN_MAP_LIMIT: usize = 2_048;

impl EngagementGuard {
    pub fn new(max_agent_chain: u32, cooldown: std::time::Duration) -> Self {
        Self {
            max_agent_chain,
            cooldown,
            chains: std::collections::HashMap::new(),
            last_engaged: std::collections::HashMap::new(),
        }
    }

    fn chain_key(channel_id: Uuid, root_hex: &str) -> String {
        format!("{channel_id}:{root_hex}")
    }

    /// Reset the chain for a thread — the owner spoke or an explicit
    /// mention re-anchored the conversation to human intent.
    pub fn reset_chain(&mut self, channel_id: Uuid, root_hex: &str) {
        self.chains.remove(&Self::chain_key(channel_id, root_hex));
    }

    /// Decide whether a thread-engaged (non-mention) turn may fire now.
    ///
    /// On `Ok(())` the chain counter and cooldown clock are advanced — call
    /// only when the turn will actually be enqueued.
    pub fn admit_thread_turn(
        &mut self,
        channel_id: Uuid,
        root_hex: &str,
        now: std::time::Instant,
    ) -> Result<u32, SuppressReason> {
        let key = Self::chain_key(channel_id, root_hex);
        let chain = self.chains.get(&key).copied().unwrap_or(0);
        if chain >= self.max_agent_chain {
            return Err(SuppressReason::ChainCap);
        }
        if let Some(last) = self.last_engaged.get(&channel_id) {
            if now.duration_since(*last) < self.cooldown {
                return Err(SuppressReason::Cooldown);
            }
        }

        if self.chains.len() >= CHAIN_MAP_LIMIT && !self.chains.contains_key(&key) {
            self.chains.clear(); // safe: worst case is a fresh chain budget
        }
        self.chains.insert(key, chain + 1);
        self.last_engaged.insert(channel_id, now);
        Ok(chain + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind, Tag};
    use std::time::{Duration, Instant};

    fn event_with_tags(kind: u16, tags: Vec<Tag>) -> nostr::Event {
        let keys = Keys::generate();
        EventBuilder::new(Kind::Custom(kind), "hi")
            .tags(tags)
            .sign_with_keys(&keys)
            .expect("sign test event")
    }

    fn e_tag(id: &str, marker: &str) -> Tag {
        Tag::parse(["e", id, "", marker]).expect("parse e tag")
    }

    #[test]
    fn reply_to_own_root_engages() {
        let mut tracker = ParticipationTracker::new();
        let channel = Uuid::new_v4();

        // Agent posts a root message (no e tags).
        let own_root = event_with_tags(9, vec![]);
        tracker.record_self_event(channel, &own_root);

        // Human replies to it.
        let reply = event_with_tags(9, vec![e_tag(&own_root.id.to_hex(), "reply")]);
        assert_eq!(
            tracker.thread_engagement_root(channel, &reply),
            Some(own_root.id.to_hex())
        );
    }

    #[test]
    fn reply_into_participated_thread_engages() {
        let mut tracker = ParticipationTracker::new();
        let channel = Uuid::new_v4();
        let root_hex = "a".repeat(64);

        // Agent replied inside someone else's thread.
        let own_reply = event_with_tags(9, vec![e_tag(&root_hex, "root")]);
        tracker.record_self_event(channel, &own_reply);

        // A different participant replies to a third message in that thread.
        let reply = event_with_tags(9, vec![e_tag(&root_hex, "root")]);
        assert_eq!(
            tracker.thread_engagement_root(channel, &reply),
            Some(root_hex)
        );
    }

    #[test]
    fn unrelated_thread_does_not_engage() {
        let mut tracker = ParticipationTracker::new();
        let channel = Uuid::new_v4();

        let own = event_with_tags(9, vec![]);
        tracker.record_self_event(channel, &own);

        let unrelated = event_with_tags(9, vec![e_tag(&"b".repeat(64), "root")]);
        assert_eq!(tracker.thread_engagement_root(channel, &unrelated), None);
    }

    #[test]
    fn participation_is_channel_scoped() {
        let mut tracker = ParticipationTracker::new();
        let channel_a = Uuid::new_v4();
        let channel_b = Uuid::new_v4();
        let root_hex = "c".repeat(64);

        let own_reply = event_with_tags(9, vec![e_tag(&root_hex, "root")]);
        tracker.record_self_event(channel_a, &own_reply);

        let reply_other_channel = event_with_tags(9, vec![e_tag(&root_hex, "root")]);
        // Same root id in another channel: thread key misses, but the own
        // event id set is channel-agnostic — only the thread path is scoped.
        assert_eq!(
            tracker.thread_engagement_root(channel_b, &reply_other_channel),
            None
        );
    }

    #[test]
    fn reactions_and_deletions_are_not_recorded() {
        let mut tracker = ParticipationTracker::new();
        let channel = Uuid::new_v4();
        let root_hex = "d".repeat(64);

        let reaction = event_with_tags(7, vec![e_tag(&root_hex, "root")]);
        let deletion = event_with_tags(5, vec![e_tag(&root_hex, "root")]);
        tracker.record_self_event(channel, &reaction);
        tracker.record_self_event(channel, &deletion);

        let reply = event_with_tags(9, vec![e_tag(&root_hex, "root")]);
        assert_eq!(tracker.thread_engagement_root(channel, &reply), None);
    }

    #[test]
    fn two_gen_set_bounds_memory_and_keeps_recent() {
        let mut set = TwoGenSet::new(8); // half = 4
        for i in 0..20 {
            set.insert(format!("id-{i}"));
        }
        // Most recent entries always retained.
        assert!(set.contains("id-19"));
        assert!(set.contains("id-16"));
        // Oldest rotated away.
        assert!(!set.contains("id-0"));
        // Bounded: at most `limit` entries live.
        assert!(set.current.len() + set.previous.len() <= 8);
    }

    #[test]
    fn chain_cap_suppresses_after_limit() {
        let mut guard = EngagementGuard::new(2, Duration::ZERO);
        let channel = Uuid::new_v4();
        let root = "e".repeat(64);
        let now = Instant::now();

        assert_eq!(guard.admit_thread_turn(channel, &root, now), Ok(1));
        assert_eq!(guard.admit_thread_turn(channel, &root, now), Ok(2));
        assert_eq!(
            guard.admit_thread_turn(channel, &root, now),
            Err(SuppressReason::ChainCap)
        );

        // Owner speaking resets the chain.
        guard.reset_chain(channel, &root);
        assert_eq!(guard.admit_thread_turn(channel, &root, now), Ok(1));
    }

    #[test]
    fn cooldown_suppresses_within_window() {
        let mut guard = EngagementGuard::new(10, Duration::from_secs(15));
        let channel = Uuid::new_v4();
        let root = "f".repeat(64);
        let t0 = Instant::now();

        assert_eq!(guard.admit_thread_turn(channel, &root, t0), Ok(1));
        assert_eq!(
            guard.admit_thread_turn(channel, &root, t0 + Duration::from_secs(5)),
            Err(SuppressReason::Cooldown)
        );
        assert_eq!(
            guard.admit_thread_turn(channel, &root, t0 + Duration::from_secs(16)),
            Ok(2)
        );
    }

    #[test]
    fn cooldown_is_per_channel() {
        let mut guard = EngagementGuard::new(10, Duration::from_secs(15));
        let root = "1".repeat(64);
        let t0 = Instant::now();

        assert_eq!(guard.admit_thread_turn(Uuid::new_v4(), &root, t0), Ok(1));
        // Different channel: independent cooldown clock.
        assert_eq!(guard.admit_thread_turn(Uuid::new_v4(), &root, t0), Ok(1));
    }

    #[test]
    fn chains_are_per_thread() {
        let mut guard = EngagementGuard::new(1, Duration::ZERO);
        let channel = Uuid::new_v4();
        let now = Instant::now();

        assert_eq!(
            guard.admit_thread_turn(channel, &"2".repeat(64), now),
            Ok(1)
        );
        assert_eq!(
            guard.admit_thread_turn(channel, &"3".repeat(64), now),
            Ok(1)
        );
        assert_eq!(
            guard.admit_thread_turn(channel, &"2".repeat(64), now),
            Err(SuppressReason::ChainCap)
        );
    }
}
