//! Durable host-final outbox record representation and validation.
//!
//! Pending records retain the exact signed event JSON required for retry. Once a
//! delivery becomes terminal, its durable record is a compact tombstone: it
//! retains only replay-fence metadata and never the event body.

use std::collections::{HashMap, HashSet};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use nostr::{Event, EventId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(super) const MAX_LOGICAL_RECEIPT_SUBMISSIONS: u8 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum HostFinalDeliveryState {
    Pending,
    AwaitingRetry,
    Accepted,
    TerminalFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct HostFinalDeliveryRecord {
    pub(super) version: u8,
    pub(super) event_id: String,
    /// Base64 of the one signed content-event serialization. This is present
    /// only while the record might be retried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) event_bytes_b64: Option<String>,
    pub(super) relay_url: String,
    pub(super) sender_pubkey: String,
    pub(super) channel_id: String,
    pub(super) trigger_event_id: String,
    /// Every event semantically consumed by the ACP prompt, including an
    /// interrupted predecessor merged into a steering batch.
    pub(super) source_event_ids: Vec<String>,
    pub(super) logical_attempts: u8,
    pub(super) state: HostFinalDeliveryState,
    /// Wall-clock instant for an interrupted one-second retry delay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) next_attempt_unix_ms: Option<u128>,
    /// Stable reason code only. Never persist prompt text, event bytes, or
    /// secrets in a diagnostic field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) diagnostic: Option<String>,
}

impl HostFinalDeliveryRecord {
    pub(super) fn accept(&mut self) {
        self.state = HostFinalDeliveryState::Accepted;
        self.compact_terminal_body();
        self.diagnostic = None;
    }

    pub(super) fn terminalize(&mut self, diagnostic: impl Into<String>) {
        self.state = HostFinalDeliveryState::TerminalFailed;
        self.compact_terminal_body();
        self.diagnostic = Some(diagnostic.into());
    }

    pub(super) fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            HostFinalDeliveryState::Accepted | HostFinalDeliveryState::TerminalFailed
        )
    }

    /// Remove the signed content body and retry timing from a terminal record.
    /// Call only after the state is terminal so an atomic replacement never
    /// leaves a retryable record without recovery bytes.
    pub(super) fn compact_terminal_body(&mut self) {
        self.event_bytes_b64 = None;
        self.next_attempt_unix_ms = None;
    }
}

#[derive(Debug, Clone)]
pub(super) struct OutboxScope {
    pub(super) relay_url: String,
    pub(super) sender_pubkey: String,
}

#[derive(Default)]
pub(super) struct OutboxIndex {
    pub(super) records: HashMap<String, HostFinalDeliveryRecord>,
    pub(super) source_event_ids: HashSet<String>,
    pub(super) load_issues: Vec<LoadIssue>,
}

#[derive(Debug, Clone)]
pub(super) struct LoadIssue {
    pub(super) event_id: Option<String>,
    pub(super) channel_id: Option<Uuid>,
    pub(super) reason: &'static str,
}

pub(super) fn encode_event_bytes(event_bytes: Vec<u8>) -> String {
    BASE64.encode(event_bytes)
}

pub(super) fn decode_event_bytes(
    record: &HostFinalDeliveryRecord,
) -> Result<Vec<u8>, &'static str> {
    record
        .event_bytes_b64
        .as_deref()
        .ok_or("record_body_missing")
        .and_then(|encoded| BASE64.decode(encoded).map_err(|_| "record_bytes_invalid"))
}

/// Validate the metadata of every record. Pending records additionally require
/// the exact signed event body. Compact terminal records intentionally do not:
/// their source IDs form the durable replay fence after delivery has ended.
///
/// A signed body on a terminal record is accepted only to migrate an old
/// version-1 tombstone safely. The caller compacts it before re-indexing.
pub(super) fn validate_record(
    record: &HostFinalDeliveryRecord,
    scope: &OutboxScope,
) -> Result<(), &'static str> {
    if record.relay_url != scope.relay_url || record.sender_pubkey != scope.sender_pubkey {
        return Err("record_scope_mismatch");
    }
    if record.version != 1
        || !is_hex_event_id(&record.event_id)
        || record.logical_attempts > MAX_LOGICAL_RECEIPT_SUBMISSIONS
        || record.source_event_ids.is_empty()
    {
        return Err("record_metadata_invalid");
    }
    let expected_event_id =
        EventId::from_hex(&record.event_id).map_err(|_| "record_event_id_invalid")?;
    let trigger_event_id =
        EventId::from_hex(&record.trigger_event_id).map_err(|_| "record_trigger_id_invalid")?;
    record
        .channel_id
        .parse::<Uuid>()
        .map_err(|_| "record_channel_id_invalid")?;
    let mut contains_trigger = false;
    for source_event_id in &record.source_event_ids {
        let source_event_id =
            EventId::from_hex(source_event_id).map_err(|_| "record_source_id_invalid")?;
        contains_trigger |= source_event_id == trigger_event_id;
    }
    if !contains_trigger {
        return Err("record_trigger_not_claimed");
    }
    if record.is_terminal() && record.next_attempt_unix_ms.is_some() {
        return Err("record_terminal_retry_scheduled");
    }

    match record.event_bytes_b64.as_deref() {
        Some(_) => validate_signed_event_body(record, expected_event_id),
        None if record.is_terminal() => Ok(()),
        None => Err("record_body_missing"),
    }
}

fn validate_signed_event_body(
    record: &HostFinalDeliveryRecord,
    expected_event_id: EventId,
) -> Result<(), &'static str> {
    let bytes = decode_event_bytes(record)?;
    let event = serde_json::from_slice::<Event>(&bytes).map_err(|_| "record_event_invalid")?;
    if event.id != expected_event_id
        || event.pubkey.to_hex() != record.sender_pubkey
        || event.verify().is_err()
    {
        return Err("record_signature_invalid");
    }
    Ok(())
}

pub(super) fn is_hex_event_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
