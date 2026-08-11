#![deny(unsafe_code)]

mod acp;
mod config;
mod engram_fetch;
mod filter;
mod final_delivery;
mod observer;
mod ompk_execution;
mod participation;
mod permission;
mod pool;
mod pool_lifecycle;
mod queue;
mod relay;
mod setup_mode;
mod usage;

pub use usage::TurnUsage;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use acp::{AcpClient, EnvVar, McpServer};
use anyhow::Result;
use buzz_core::kind::{
    KIND_AGENT_OBSERVER_FRAME, KIND_MEMBER_ADDED_NOTIFICATION, KIND_MEMBER_REMOVED_NOTIFICATION,
    KIND_STREAM_MESSAGE, KIND_STREAM_REMINDER, KIND_WORKFLOW_APPROVAL_REQUESTED,
};
use buzz_core::observer::{
    decrypt_observer_payload, encrypt_observer_payload, OBSERVER_AGENT_TAG, OBSERVER_FRAME_CONTROL,
    OBSERVER_FRAME_TAG, OBSERVER_FRAME_TELEMETRY, OBSERVER_MAX_PLAINTEXT_LEN,
};
use clap::Parser;
use config::{
    AuthAgentArgs, AuthMethodsArgs, AuthenticateArgs, Config, DedupMode, ModelsArgs,
    MultipleEventHandling, RespondTo, SubscribeMode,
};
use filter::SubscriptionRule;
use futures_util::FutureExt;
use nostr::{PublicKey, ToBech32};
use permission::{OwnerPermissionDecision, PermissionBinding, PermissionDispatchStatus};
use pool::{
    AgentPool, ControlSignal, IdleSwitchResult, OwnedAgent, PromptContext, PromptOutcome,
    PromptResult, PromptSource, SessionState, TimeoutKind,
};
use pool_lifecycle::PoolLifecycle;
use queue::{CancelReason, EventQueue, FlushBatch, QueuedEvent, ThreadTags};
use relay::{HarnessRelay, RelayEventPublisher};
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

/// Check if argv[1] matches a subcommand name, before any clap parsing.
///
/// This avoids clap rejecting harness flags (like `--private-key`) that aren't
/// declared on the subcommand's `Parser`. The `models` path has its own
/// dedicated parser; the default path uses the existing `CliArgs`.
///
/// **Constraint**: subcommand must be argv[1] â€” flags before the subcommand
/// name (e.g., `buzz-acp --verbose models`) are not supported.
fn is_subcommand(name: &str) -> bool {
    std::env::args().nth(1).map(|a| a == name).unwrap_or(false)
}

/// Timeout for lightweight helper subcommands (spawn + initialize + model/method probes).
const MODELS_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for `buzz-acp authenticate`. Browser-based vendor auth can require
/// human interaction, so it must not share the short probe timeout.
const AUTHENTICATE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Publish a kind:20001 presence update event via the WebSocket connection.
///
/// Ephemeral kinds (20000-29999) are rejected by the HTTP bridge, so presence
/// updates must be routed through the WS path.
///
/// Content is a bare status string (`"online"`, `"away"`, `"offline"`) matching
/// the desktop client's format. The relay stores this in Redis and synthesizes
/// it back on presence queries.
async fn publish_presence(
    publisher: &relay::RelayEventPublisher,
    keys: &nostr::Keys,
    status: &str,
) -> Result<(), relay::RelayError> {
    use buzz_core::kind::KIND_PRESENCE_UPDATE;
    use nostr::{EventBuilder, Kind};

    let event = EventBuilder::new(Kind::Custom(KIND_PRESENCE_UPDATE as u16), status)
        .tags([])
        .sign_with_keys(keys)
        .map_err(|e| relay::RelayError::Http(format!("presence sign error: {e}")))?;
    publisher.publish_event(event).await?;
    Ok(())
}

fn emit_runtime_lifecycle(
    observer: Option<&observer::ObserverHandle>,
    start_nonce: &str,
    pubkey: &str,
    relay_url: &str,
    lifecycle: &str,
    error: Option<&str>,
) {
    if let Some(observer) = observer {
        observer.emit(
            "managed_agent_runtime_lifecycle",
            None,
            &observer::ObserverContext::default(),
            serde_json::json!({
                "pubkey": pubkey,
                "relayUrl": relay_url,
                "startNonce": start_nonce,
                "lifecycle": lifecycle,
                "error": error,
            }),
        );
    }
}

/// Emit an `engagement_decision` observer frame (thread-engagement telemetry).
///
/// These frames are the dogfooding instrument for non-mention engagement:
/// every fired or suppressed thread turn is recorded with its chain depth or
/// suppress reason. Viewable in the desktop session viewer's "Raw ACP
/// activity" feed (the default Activity transcript drops this frame type) or
/// the Harness Log panel.
fn emit_engagement_decision(
    observer: Option<&observer::ObserverHandle>,
    buzz_event: &relay::BuzzEvent,
    decision: &str,
    chain_depth: Option<u32>,
    reason: Option<&str>,
) {
    let Some(observer) = observer else {
        return;
    };
    let context = observer::context_for(Some(buzz_event.channel_id), None, None);
    observer.emit(
        "engagement_decision",
        None,
        &context,
        serde_json::json!({
            "eventId": buzz_event.event.id.to_hex(),
            "channelId": buzz_event.channel_id.to_string(),
            "author": buzz_event.event.pubkey.to_hex(),
            "decision": decision,
            "chainDepth": chain_depth,
            "reason": reason,
        }),
    );
}

/// Replay window for participation rehydration at startup (24 h).
const PARTICIPATION_REHYDRATE_WINDOW_SECS: u64 = 86_400;
/// Cap on rehydrated self-authored events.
const PARTICIPATION_REHYDRATE_LIMIT: usize = 200;
/// Hard timeout for the rehydration query â€” startup must not hang on it.
const PARTICIPATION_REHYDRATE_TIMEOUT: Duration = Duration::from_secs(5);

/// Seed the participation tracker from the agent's recently authored events.
///
/// Best-effort: failure degrades to "explicit mention required again" in
/// threads older than process start, never to over-engagement.
async fn rehydrate_participation(
    participation: &mut participation::ParticipationTracker,
    rest: &relay::RestClient,
    agent_pubkey_hex: &str,
) {
    let author = match nostr::PublicKey::from_hex(agent_pubkey_hex) {
        Ok(pk) => pk,
        Err(e) => {
            tracing::warn!("participation rehydrate: bad agent pubkey: {e}");
            return;
        }
    };
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Relay queries must carry explicit kinds (p-gate); the timeline trio is
    // the same set the synthesized rules subscribe to.
    let filter = nostr::Filter::new()
        .authors([author])
        .kinds([
            nostr::Kind::Custom(KIND_STREAM_MESSAGE as u16),
            nostr::Kind::Custom(KIND_WORKFLOW_APPROVAL_REQUESTED as u16),
            nostr::Kind::Custom(KIND_STREAM_REMINDER as u16),
        ])
        .since(nostr::Timestamp::from(
            now_secs.saturating_sub(PARTICIPATION_REHYDRATE_WINDOW_SECS),
        ))
        .limit(PARTICIPATION_REHYDRATE_LIMIT);

    let response =
        match tokio::time::timeout(PARTICIPATION_REHYDRATE_TIMEOUT, rest.query(&[filter])).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::warn!("participation rehydrate: query failed: {e}");
                return;
            }
            Err(_) => {
                tracing::warn!("participation rehydrate: query timed out");
                return;
            }
        };

    let Some(events) = response.as_array() else {
        tracing::warn!("participation rehydrate: unexpected response shape");
        return;
    };
    let mut recorded = 0usize;
    for raw in events {
        let Ok(event) = serde_json::from_value::<nostr::Event>(raw.clone()) else {
            continue;
        };
        let Some(channel_id) = event.tags.iter().find_map(|tag| {
            let s = tag.as_slice();
            if s.first().map(|k| k.as_str()) == Some("h") {
                s.get(1).and_then(|v| v.parse::<Uuid>().ok())
            } else {
                None
            }
        }) else {
            continue;
        };
        participation.record_self_event(channel_id, &event);
        recorded += 1;
    }
    tracing::info!(recorded, "participation rehydrated from recent history");
}

/// Resolve the agent's owner pubkey at startup.
///
/// Priority:
/// 1. `BUZZ_AUTH_TAG` env var â€” NIP-OA attestation signed by the owner.
///    Verified against the agent's own pubkey to extract the owner pubkey.
/// 2. `--agent-owner` CLI flag / `BUZZ_ACP_AGENT_OWNER` env var.
fn resolve_agent_owner(config: &Config) -> Option<String> {
    // Try BUZZ_AUTH_TAG first (NIP-OA attestation).
    if let Ok(auth_tag) = std::env::var("BUZZ_AUTH_TAG") {
        if !auth_tag.is_empty() {
            let agent_pk = config.keys.public_key();
            match buzz_sdk::nip_oa::verify_auth_tag(&auth_tag, &agent_pk) {
                Ok(owner_pk) => {
                    let owner_hex = owner_pk.to_hex().to_ascii_lowercase();
                    tracing::info!("owner resolved from BUZZ_AUTH_TAG: {owner_hex}");
                    return Some(owner_hex);
                }
                Err(e) => {
                    tracing::warn!("BUZZ_AUTH_TAG verification failed: {e} â€” falling back");
                }
            }
        }
    }

    // Fall back to --agent-owner config.
    config.agent_owner.clone()
}

/// Cache for the agent's owner pubkey.
///
/// Owner is now provided via `--agent-owner` config flag (no REST lookup).
/// Cache for the agent's owner pubkey + sibling lookups.
///
/// Siblings are other agents whose NIP-OA auth tag proves the same owner.
/// Lookup results are cached for the process lifetime (attestations are immutable).
struct OwnerCache {
    pubkey: Option<String>,
    /// author_hex â†’ is_sibling (true = same owner, false = not)
    siblings: std::sync::Mutex<HashMap<String, bool>>,
}

impl OwnerCache {
    fn new(initial: Option<String>) -> Self {
        Self {
            pubkey: initial,
            siblings: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Return the cached owner pubkey.
    fn get(&self) -> Option<&str> {
        self.pubkey.as_deref()
    }

    /// Check if author is a known sibling (cached result).
    fn is_known_sibling(&self, author: &str) -> Option<bool> {
        self.siblings.lock().ok()?.get(author).copied()
    }

    /// Cache a sibling lookup result.
    fn cache_sibling(&self, author: String, is_sibling: bool) {
        if let Ok(mut map) = self.siblings.lock() {
            // Cap at 256 entries to prevent unbounded growth.
            if map.len() >= 256 {
                map.clear();
            }
            map.insert(author, is_sibling);
        }
    }
}

/// Check if `author` is the owner OR a sibling (same owner via NIP-OA).
///
/// For unknown authors, queries their kind:0 profile to extract the NIP-OA
/// auth tag and verify the owner matches. Result is cached.
async fn is_owner_or_sibling(
    author: &str,
    owner_cache: &OwnerCache,
    rest_client: &relay::RestClient,
) -> bool {
    let my_owner = match owner_cache.get() {
        Some(o) => o,
        None => return false, // no owner configured â€” fail closed
    };

    // Direct owner check.
    if author == my_owner {
        return true;
    }

    // Check sibling cache.
    if let Some(cached) = owner_cache.is_known_sibling(author) {
        return cached;
    }

    // Query the author's kind:0 profile to check for NIP-OA auth tag.
    let is_sibling = check_sibling_via_profile(author, my_owner, rest_client).await;
    owner_cache.cache_sibling(author.to_string(), is_sibling);
    is_sibling
}

/// Inbound author gate decision: does this author's event fire a turn?
///
/// Coarse security policy applied before subscription rules. Both `OwnerOnly`
/// and `Allowlist` accept the owner and same-owner siblings; `Allowlist`
/// additionally accepts the explicit external pubkey list.
///
/// # DM hardening (`is_dm`)
///
/// Clients auto-p-tag every DM participant, so in a DM *any* participant's
/// message looks like a mention and would fire a turn. Combined with
/// agent-initiated DMs (the agent can be asked to DM a third party), that
/// turns `anyone`/`allowlist` modes into transitive access grants: whoever
/// lands in a DM with the agent can prompt it. To close that hole, when
/// `is_dm` is true only the owner and cryptographically verified same-owner
/// siblings may fire a turn â€” the explicit allowlist and `anyone` mode do
/// NOT apply inside DMs. `Nobody` still drops everything. Callers must
/// resolve `is_dm` fail-closed: unknown channel type â‡’ treat as DM.
async fn author_allowed(
    respond_to: &RespondTo,
    allowlist: &HashSet<String>,
    author: &str,
    is_dm: bool,
    owner_cache: &OwnerCache,
    rest_client: &relay::RestClient,
) -> bool {
    if is_dm {
        return match respond_to {
            RespondTo::Nobody => false,
            _ => is_owner_or_sibling(author, owner_cache, rest_client).await,
        };
    }
    match respond_to {
        RespondTo::Anyone => true,
        RespondTo::Nobody => false,
        RespondTo::OwnerOnly => is_owner_or_sibling(author, owner_cache, rest_client).await,
        RespondTo::Allowlist => {
            allowlist.contains(author)
                || is_owner_or_sibling(author, owner_cache, rest_client).await
        }
    }
}

/// Resolve whether `channel_id` is a DM, for the inbound author gate.
///
/// Resolution order:
/// 1. Startup discovery metadata (`startup_info`) â€” covers channels known at
///    process start.
/// 2. Per-loop resolution cache (`cache`) â€” covers channels resolved since.
/// 3. Lazy REST fetch of the channel's kind:39000 metadata â€” covers channels
///    the agent was added to *after* startup (the exploit path: an
///    agent-initiated DM is exactly such a channel).
///
/// Fail-closed: if the fetch fails or times out, the channel is treated as a
/// DM for this event and the result is NOT cached, so a later event retries
/// the fetch instead of pinning a mis-classification.
pub(crate) async fn is_dm_channel(
    channel_id: Uuid,
    channel_info: &pool::ChannelInfoResolver,
) -> bool {
    match channel_info.resolve(channel_id).await {
        Some(info) => info.channel_type == "dm",
        None => {
            tracing::warn!(
                channel_id = %channel_id,
                "channel type unresolved â€” treating as DM for author gate (fail closed)"
            );
            true
        }
    }
}

/// Query an author's kind:0 profile and check if their NIP-OA auth tag
/// proves the same owner as us.
async fn check_sibling_via_profile(
    author: &str,
    expected_owner: &str,
    rest_client: &relay::RestClient,
) -> bool {
    let filter = nostr::Filter::new()
        .kind(nostr::Kind::Metadata)
        .author(match nostr::PublicKey::from_hex(author) {
            Ok(pk) => pk,
            Err(_) => return false,
        })
        .limit(1);

    let resp = match tokio::time::timeout(Duration::from_mill×Nvç{h‘éì¶»§q«^u]\Ú×ÚYHÛÛš›Ú[—ÜÙ]œÜ]ÛŠ\Ş[˜ÈßJKšY

NÂˆÛÛ\Ú×ÛX\Û]]

Kš[œÙ\
ˆ\Ú×ÚYˆÜ˜]NœÛÛ•\ÚÓY]HÂˆYÙ[Ú[™^ˆˆÚ[›™[ÚYˆ›Û™Kˆ\›—ÚYˆ\İ]\›‹ZY‹×Üİš[™Ê
Kˆ™XÛİ™\˜X›WØ˜]Úˆ›Û™KˆÛÛ›Ûİˆ›Û™KˆİY\—İˆ›Û™KˆKˆ
NÂˆ]]]]Y]YHH]™[]Y]YN›™]ÊÛÛ™šYÎ‘Y\[ÙN”]Y]YJNÂˆ]ÛÛ™šYÈH\İØÛÛ™šYÊ
NÂˆ]]]X\™X]Ú[—Ù›YÚH˜[ÙNÂˆ]™[[İ™YØÚ[›™[ÈHİ˜ÛÛXİ[ÛœÎ’\ÚÙ]›™]Ê
NÂˆ]]]Ü˜\ÚÚ\İÜHH™XÈVÔÛİÚ\˜İZ]ÂˆÜ˜\Úİ[Y\Îˆ™XÎ›™]Ê
KˆÜ[—İ[[ˆ›Û™Kˆ™\Ü]Û—Ú[—Ù›YÚˆ˜[ÙKˆWNÂˆ]
™\Ü]Û—İÜ™\Ü]Û—Ü
HH\ØÎ˜Ú[›™[

NÂˆ]]]™\Ü]Û—İ\ÚÜÈHÚÚ[Î\ÚÎ’›Ú[”Ù]›™]Ê
NÂˆ]™\İ[H›Û\™\İ[ÂˆYÙ[ˆÛİ\˜ÙNˆ›Û\Ûİ\˜ÙNÚ[›™[
Ú[›™[ÚY
Kˆ\›—ÚYˆ\İ]\›‹ZY‹×Üİš[™Ê
Kˆİ]ÛÛYNˆ›Û\İ]ÛÛYN‘\œ›ÜŠ]]Ù\œ›ÜŠKˆ˜]ÚˆÛÛYJ˜]Ú
KˆNÂˆ[™WÜ›Û\Ü™\İ[
ˆ	›]]ÛÛˆ	›]]]Y]YKˆ	˜ÛÛ™šYËˆ™\İ[ˆ	›]]X\™X]Ú[—Ù›YÚˆ	œ™[[İ™YØÚ[›™[Ëˆ	›]]Ü˜\ÚÚ\İÜKˆ	œ™\Ü]Û—İˆ	›]]™\Ü]Û—İ\ÚÜËˆ›Û™Kˆ›Û™Kˆ
NÂ‚ˆËÈH˜]Ú]\İ›İ™H™\]Y]YYˆ[™[™×ØÚ[›™[È™]\›œÈ‚ˆ\ÜÙ\Ù\HJˆ]Y]YKœ[™[™×ØÚ[›™[Ê
Kˆˆ˜]]\œ›Üˆ]\İXY[]\ˆ[[YYX][H8 %˜]Ú]\İ›İ™H™\]Y]YY‚ˆ
NÂˆ\ÜÙ\Ù\HJˆ]Y]YKœ]Y]YYÙ]™[ØÛİ[
	˜Ú[›™[ÚY
Kˆˆ˜]]\œ›Üˆ]\İXY[]\ˆ[[YYX][H8 %›È]™[ÈÚİ[™H[™[™È‚ˆ
NÂˆB‚ˆËËÈH›Û‹X]]\XØ][Ûˆ\œ›Üˆ
K™Ëˆ\ØYÙHÜ™Y]ÊH]\İİ[›ÛİÈBˆËËÈİ[™\™™\]Y]YH]ÛÈÙ^IÜÈ™Z]š[Üˆ\È[˜Ú[™ÙY‚ˆÖİÚÚ[Î\İBˆ\Ş[˜È›ˆ›Û—Ø]]Ø\XØ][Û—Ù\œ›Ü—Ú\×Ü™\]Y]YY

HÂˆ]Ù^\ÈH›Üİ’Ù^\Î™Ù[™\˜]J
NÂˆ]]™[H›Üİ‘]™[Z[\›™]Ê›Üİ’Ú[™İ\İÛJJK\İŠBˆœÚYÛ—İÚ]ÚÙ^\Ê	šÙ^\ÊBˆ[Ü˜\

NÂˆ]Ú[›™[ÚYH]ZY•]ZY›™]×İ

NÂˆ]˜]ÚH›\Ú˜]ÚÂˆÚ[›™[ÚYˆ]™[Îˆ™XÈVĞ˜]Ú]™[Âˆ]™[ˆ›Û\İYÎˆ\İ‹š[Ê
Kˆ™XÙZ]™YØ]ˆİ[YN’[œİ[››İÊ
KˆWKˆØ[˜Ù[YÙ]™[Îˆ™XÈV×KˆØ[˜Ù[Ü™X\ÛÛˆ›Û™KˆNÂ‚ˆËÈ\ØYÙKXÜ™Y]È\œ›Üˆ8 %YÙ[\œ›Üˆ]“Õ[ˆ]]\œ›Ü‹‚ˆ]\ØYÙWÙ\œ›ÜˆHXÜXÜ\œ›ÜYÙ[\œ›ÜˆÂˆÛÙNˆLÌŒˆY\ÜØYÙNˆ•\ØYÙHÜ™Y]È™\]Z\™Y›ÜˆSHÛÛ^‹×Üİš[™Ê
KˆNÂ‚ˆ]YÙ[H[[^WØYÙ[

K˜]ØZ]Âˆ]]]ÛÛHYÙ[ÛÛ™œ›ÛWÜÛİÊ™XÈVÓ›Û™WJNÂˆ]\Ú×ÚYHÛÛš›Ú[—ÜÙ]œÜ]ÛŠ\Ş[˜ÈßJKšY

NÂˆÛÛ\Ú×ÛX\Û]]

Kš[œÙ\
ˆ\Ú×ÚYˆÜ˜]NœÛÛ•\ÚÓY]HÂˆYÙ[Ú[™^ˆˆÚ[›™[ÚYˆ›Û™Kˆ\›—ÚYˆ\İ]\›‹ZY‹×Üİš[™Ê
Kˆ™XÛİ™\˜X›WØ˜]Úˆ›Û™KˆÛÛ›Ûİˆ›Û™KˆİY\—İˆ›Û™KˆKˆ
NÂˆ]]]]Y]YHH]™[]Y]YN›™]ÊÛÛ™šYÎ‘Y\[ÙN”]Y]YJNÂˆ]ÛÛ™šYÈH\İØÛÛ™šYÊ
NÂˆ]]]X\™X]Ú[—Ù›YÚH˜[ÙNÂˆ]™[[İ™YØÚ[›™[ÈHİ˜ÛÛXİ[ÛœÎ’\ÚÙ]›™]Ê
NÂˆ]]]Ü˜\ÚÚ\İÜHH™XÈVÔÛİÚ\˜İZ]ÂˆÜ˜\Úİ[Y\Îˆ™XÎ›™]Ê
KˆÜ[—İ[[ˆ›Û™Kˆ™\Ü]Û—Ú[—Ù›YÚˆ˜[ÙKˆWNÂˆ]
™\Ü]Û—İÜ™\Ü]Û—Ü
HH\ØÎ˜Ú[›™[

NÂˆ]]]™\Ü]Û—İ\ÚÜÈHÚÚ[Î\ÚÎ’›Ú[”Ù]›™]Ê
NÂˆ]™\İ[H›Û\™\İ[ÂˆYÙ[ˆÛİ\˜ÙNˆ›Û\Ûİ\˜ÙNÚ[›™[
Ú[›™[ÚY
Kˆ\›—ÚYˆ\İ]\›‹ZY‹×Üİš[™Ê
Kˆİ]ÛÛYNˆ›Û\İ]ÛÛYN‘\œ›ÜŠ\ØYÙWÙ\œ›ÜŠKˆ˜]ÚˆÛÛYJ˜]Ú
KˆNÂˆ[™WÜ›Û\Ü™\İ[
ˆ	›]]ÛÛˆ	›]]]Y]YKˆ	˜ÛÛ™šYËˆ™\İ[ˆ	›]]X\™X]Ú[—Ù›YÚˆ	œ™[[İ™YØÚ[›™[Ëˆ	›]]Ü˜\ÚÚ\İÜKˆ	œ™\Ü]Û—İˆ	›]]™\Ü]Û—İ\ÚÜËˆ›Û™Kˆ›Û™Kˆ
NÂ‚ˆËÈ›Û‹X]]\XØ][Ûˆ\œ›Üˆ˜]ÚTÈ™\]Y]YY
š\œİ][\™]HYÙ]ˆ
K‚ˆ\ÜÙ\Ù\HJˆ]Y]YKœ[™[™×ØÚ[›™[Ê
KˆKˆ››Û‹X]]\XØ][Ûˆ\œ›Üˆ]\İ™\]Y]YHH˜]Ú›Üˆ™]H‚ˆ
NÂˆ\ÜÙ\Ù\HJˆ]Y]YKœ]Y]YYÙ]™[ØÛİ[
	˜Ú[›™[ÚY
KˆKˆ››Û‹X]]\XØ][Ûˆ\œ›Üˆ]\İ™\Ù\™HH]™[›Üˆ™]H‚ˆ
NÂˆBŸB‚ˆÖØÙ™Ê\İ
WB›[ÙØœÙ\™\—Ü^[ØYİš[Wİ\İÈÂˆ\ÙHİ\\ŠÂ‚ˆ›ˆ]™[İÚ]Ü^[ØY
Ú[™ˆ	œİ‹^[ØYˆÙ\™WÚœÛÛ•˜[YJHOˆØœÙ\™\“ØœÙ\™\‘]™[ÂˆØœÙ\™\“ØœÙ\™\‘]™[ÂˆÙ\NˆKˆ[Y\İ[\ˆŒŒ‹L‹LM•ŒŒˆ‹×Üİš[™Ê
KˆÚ[™ˆÚ[™×Üİš[™Ê
KˆYÙ[Ú[™^ˆÛÛYJ
KˆÚ[›™[ÚYˆÛÛYJŒLLLLLLLKLLLLKLLLLKLLLLKLLLLLLLLLLLLH‹×Üİš[™Ê
JKˆÙ\ÜÚ[Û—ÚYˆÛÛYJœÙ\ÜËLH‹×Üİš[™Ê
JKˆ\›—ÚYˆÛÛYJ\›‹LH‹×Üİš[™Ê
JKˆİ\YØ]ˆ›Û™Kˆ^[ØYˆBˆB‚ˆ›ˆÙ\šX[^™Y
]™[ˆ	›ØœÙ\™\“ØœÙ\™\‘]™[
HOˆİš[™ÈÂˆÙ\™WÚœÛÛ×Üİš[™Ê]™[
K[Ü˜\

BˆB‚ˆÖİ\İBˆ›ˆ\İİ[™\—ØYÙ]Ùœ˜[YWÜ\ÜÙ\×İ›İYÚØ]WÚY[XØ[

HÂˆ]]]]™[H]™[İÚ]Ü^[ØY
˜XÜÜ™XY‹Ù\™WÚœÛÛšœÛÛˆJÈ˜›ÙHˆœÛX[ˆJJNÂˆ]™Y›Ü™HHÙ\šX[^™Y
	™]™[
NÂˆš]ÛØœÙ\™\—Ù]™[İ×ØYÙ]
	›]]]™[
NÂˆ\ÜÙ\Ù\HJˆÙ\šX[^™Y
	™]™[
Kˆ™Y›Ü™Kˆ[™\‹XYÙ]œ˜[YH]\İ›İ™H]]]Y‚ˆ
NÂˆB‚ˆÖİ\İBˆ›ˆ\İÜÚ[™ÛWÙÚX[ÛXY—Ú\×Ù[YYİ×Ùš]İÚ]Ù[™[ÜWÚ[Xİ

HÂˆ]šYÈH‹œ™\X]
LÌ
NÂˆ]]]]™[H]™[İÚ]Ü^[ØY
˜XÜÜ™XY‹Ù\™WÚœÛÛšœÛÛˆJÈ˜›ÙHˆšYÈJJNÂˆš]ÛØœÙ\™\—Ù]™[İ×ØYÙ]
	›]]]™[
NÂ‚ˆ\ÜÙ\JˆÙ\šX[^™Y
	™]™[
K›[Š
HHĞ”ÑT•‘T—ÓPVÔRS•VÓS‹ˆ™œ˜[YH]\İš]Y\ˆš[[Z[™È‚ˆ
NÂˆËÈ[™[ÜH[Xİ‚ˆ\ÜÙ\Ù\HJ]™[šÚ[™˜XÜÜ™XYŠNÂˆ\ÜÙ\Ù\HJ]™[\›—ÚY˜\×Ù\™YŠ
KÛÛYJ\›‹LHŠJNÂˆ\ÜÙ\Ù\HJˆ]™[˜Ú[›™[ÚY˜\×Ù\™YŠ
KˆÛÛYJŒLLLLLLLKLLLLKLLLLKLLLLKLLLLLLLLLLLLHŠBˆ
NÂˆ\ÜÙ\Ù\HJ]™[œÙ\KJNÂ‚ˆ]XYˆH]™[œ^[ØYÈ˜›ÙH—K˜\×ÜİŠ
K[Ü˜\

NÂˆ\ÜÙ\JˆXY‹œİ\×İÚ]
	ˆ‹œ™\X]
Ğ”ÑT•‘T—ÓPQ—Ô‘URS—Ğ–UTÊJKˆšXY™]Z[™Y‚ˆ
NÂˆ\ÜÙ\JˆXY‹™[™×İÚ]
	ˆ‹œ™\X]
Ğ”ÑT•‘T—ÓPQ—Ô‘URS—Ğ–UTÊJKˆZ[™]Z[™Y‚ˆ
NÂˆËÈˆ[ˆHX\šÙ\ˆ\ÈUÈ]\È™[[İ™YˆÜšYÚ[˜[[ˆZ[\È™]Z[™Y[‹‚ˆ]™[[İ™YHLÌHXY‹˜Ú\œÊ
K™š[\Šß
˜ÈOH	Ş	ÊK˜Ûİ[

NÂˆ\ÜÙ\JˆXY‹˜ÛÛZ[œÊ	™›Ü›X]J¸ )–Ù[YYÜ™[[İ™YH]\×x )ˆŠJKˆ›X\šÙ\ˆ™\ÜÈ˜]È]\È™[[İ™Y‚ˆ
NÂˆB‚ˆÖİ\İBˆ›ˆ\İÛ][WØ›ØÚ×Ü›Û\Ü™]Z[œ×Ù]™\WÜÙXİ[Û—ÚXY\—ØY\—Ù[\Ú[ÛŠ
HÂˆËÈH™X[Ù\ÜÚ[Û‹Ü›Û\š^ˆ›Ü›X]Ü›Û\›İÈ[Z]ÈÛ™H›ØÚÈ\‚ˆËÈÙXİ[Û‹ÛÈHØœÙ\™\ˆ^[ØY\È\˜[\Ëœ›Û\HŞİ^ˆ–Ğ˜\ÙWx )ˆŸKˆËÈİ^ˆ–ĞYÙ[Y[[ÜH8 %ÛÜ™Wx )ˆŸK8 )ˆİ^ˆ–ÔŞˆ]™[ˆ8 )—x )YÙOˆŸWK‚ˆËÈ[ˆİ™\œÚ^™YÙXİ[Ûˆ\È]ÈİÛˆXY‹ÛÈ[Y[™È]È›ÙHÙY\ÈBˆËÈXY‰ÜÈXYLÌ
ÚXÚ™YÚ[œÈÚ]HÙXİ[Û‰ÜÈÒXY\—H[™JH8 %]™\BˆËÈXY\ˆİ\š]™\ËÛÈH\ÚİÜ”›Û\ÛÛ^ˆ[™[Ûİ[È[H[‚ˆËÈ\È\ÈH™YÜ™\ÜÚ[ÛˆHÚ[™ÛKY˜][XYˆÚ\HØ]\ÙY
H˜Z[[™ÂˆËÈÔŞˆ]™[HXY\ˆ™[[ÈH[YYZYH[™HÛİ[ÛÛ\ÙYˆËÈÈJK‚ˆ]ÙXİ[ÛœÈHÂˆ–Ğ˜\ÙWW[İH\™HH[[YÙ[‹×Üİš[™Ê
Kˆ–ÔŞ\İ[WWœ\œÛÛ˜H^‹×Üİš[™Ê
Kˆ–ĞYÙ[Y[[ÜH8 %ÛÜ™WWœ™[Y[X™\ˆ\È‹×Üİš[™Ê
Kˆ–ĞÛÛ^W”ØÛÜNˆ™XY‹×Üİš[™Ê
KˆËÈHšYÙÙ\š[™È]™[›ÙKİ™\œÚ^™YÛˆ]ÈİÛ‹‚ˆ›Ü›X]J–ÔŞˆ]™[ˆY[[Û—WÛÛ[ˆßH‹‘H‹œ™\X]
LÌ
JKˆNÂˆ]›ØÚ×Ü™YœÎˆ™XÏ	œİˆHÙXİ[ÛœËš]\Š
K›X\
İš[™Î˜\×ÜİŠK˜ÛÛXİ

NÂˆËÈZ\œ›ÜˆHÚ\™HÚ\HZ[Ü›Û\Ü\˜[\È›ÙXÙ\ÎˆXXÚ›ØÚÈ\È]ÂˆËÈİÛˆİ\Nˆ^‹^HXYˆ[™\ˆ\˜[\Ëœ›Û\‚ˆ]›Û\Ø›ØÚÜÎˆ™XÏÙ\™WÚœÛÛ•˜[YOˆH›ØÚ×Ü™YœÂˆš]\Š
Bˆ›X\
^Ù\™WÚœÛÛšœÛÛˆJÈ\Hˆ^‹^ˆ^JJBˆ˜ÛÛXİ

NÂˆ]]]]™[H]™[İÚ]Ü^[ØY
ˆ˜XÜİÜš]H‹ˆÙ\™WÚœÛÛšœÛÛˆJÂˆ›Y]ÙˆœÙ\ÜÚ[Û‹Ü›Û\‹ˆœ\˜[\ÈˆÈœÙ\ÜÚ[Û’YˆœÙ\ÜËLH‹œ›Û\ˆ›Û\Ø›ØÚÜÈKˆJKˆ
NÂˆ\ÜÙ\JˆÙ\šX[^™Y
	™]™[
K›[Š
HˆĞ”ÑT•‘T—ÓPVÔRS•VÓS‹ˆœ™XÛÛ™][Ûˆİ™\œÚ^™Y]™[›ÙH\Ú\ÈHœ˜[YHİ™\ˆHØ\‚ˆ
NÂ‚ˆš]ÛØœÙ\™\—Ù]™[İ×ØYÙ]
	›]]]™[
NÂ‚ˆ\ÜÙ\JˆÙ\šX[^™Y
	™]™[
K›[Š
HHĞ”ÑT•‘T—ÓPVÔRS•VÓS‹ˆ™œ˜[YH]\İš]Y\ˆš[[Z[™È‚ˆ
NÂˆ]›ØÚÜÈH]™[œ^[ØYÈœ\˜[\È—VÈœ›Û\—Bˆ˜\×Ø\œ˜^J
Bˆ™^Xİ
œ›Û\\œ˜^Hİ\š]™\ÈŠNÂˆ]^Îˆ™XÏ	œİˆH›ØÚÜËš]\Š
K›X\
Ÿ–È^—K˜\×ÜİŠ
K[Ü˜\

JK˜ÛÛXİ

NÂˆ›ÜˆXY\ˆ[ˆÂˆ–Ğ˜\ÙWH‹ˆ–ÔŞ\İ[WH‹ˆ–ĞYÙ[Y[[ÜH8 %ÛÜ™WH‹ˆ–ĞÛÛ^H‹ˆ–ÔŞˆ]™[ˆY[[Û—H‹ˆHÂˆ\ÜÙ\Jˆ^Ëš]\Š
K˜[Jœİ\×İÚ]
XY\ŠJKˆœÙXİ[ÛˆXY\ˆÚXY\ŸH]\İİ\š]™H]HXYÙˆ]ÈİÛˆ›ØÚÈ‚ˆ
NÂˆBˆËÈHİ™\œÚ^™Y]™[›ÙHØ\È[YY[ˆXÙH
XY\ˆÙ\ZYHİ]
K‚ˆ]]™[Ø›ØÚÈH^Âˆš]\Š
Bˆ™š[™
œİ\×İÚ]
–ÔŞˆ]™[ˆY[[Û—HŠJBˆ[Ü˜\

NÂˆ\ÜÙ\Jˆ]™[Ø›ØÚË˜ÛÛZ[œÊ¸ )–Ù[YYŠKˆHİ™\œÚ^™Y]™[›ÙH\È[YY›İ›ÜY‚ˆ
NÂˆB‚ˆÖİ\İBˆ›ˆ\İÛ][WÛXY—Ù[Y\×Û\™Ù\İÜÚš[šØX›WÙš\œİØ[™ÜİÜ×İÚ[—Ú]Ùš]Ê
HÂˆËÈÛ™HXYˆ[Û™Hİ™\ˆHØ\ÈHÙXÛÛ™ÛX[\‹X]\İ[[\™ÙHXY‹‚ˆËÈ[Y[™ÈHšYÙÙ\İÚİ[İY™šXÙKX]š[™ÈHÛX[\ˆ[Xİ‚ˆ]]]]™[H]™[İÚ]Ü^[ØY
ˆ˜XÜİÜš]H‹ˆÙ\™WÚœÛÛšœÛÛˆJÂˆšYÙHˆ˜H‹œ™\X]
LÌ
Kˆ›YY][Hˆ˜ˆ‹œ™\X]
ŒÌ
KˆJKˆ
NÂˆš]ÛØœÙ\™\—Ù]™[İ×ØYÙ]
	›]]]™[
NÂ‚ˆ\ÜÙ\JÙ\šX[^™Y
	™]™[
K›[Š
HHĞ”ÑT•‘T—ÓPVÔRS•VÓSŠNÂˆ\ÜÙ\Jˆ]™[œ^[ØYÈšYÙH—K˜\×ÜİŠ
K[Ü˜\

K˜ÛÛZ[œÊ¸ )–Ù[YYŠKˆH\™Ù\İXYˆ\È[YY‚ˆ
NÂˆ\ÜÙ\Ù\HJˆ]™[œ^[ØYÈ›YY][H—K˜\×ÜİŠ
K[Ü˜\

K›[Š
KˆŒÌˆHÛX[\ˆXYˆ\ÈY[İXÚYÛ˜ÙHHœ˜[YHš]È‚ˆ
NÂˆB‚ˆÖİ\İBˆ›ˆ\İØÛØ[\ØÙYØÚ[š×Û™\İYÛXY—Ú\×Ü™XXÚYØWÜ™Xİ\œÚ]™WİØ[Ê
HÂˆËÈHÛØ[\ØÙYXÚ[šÈšYÈXYˆ]™\È]\˜[\Ë\]K˜ÛÛ[^ˆËÈ›İHÜ[]™[šY[8 %HØ[È]\İ™Xİ\œÙHÈ™XXÚ]‚ˆ]šYÈHˆ‹œ™\X]
Ì
NÂˆ]]]]™[H]™[İÚ]Ü^[ØY
ˆœÙ\ÜÚ[Û—İ\]H‹ˆÙ\™WÚœÛÛšœÛÛˆJÂˆœ\˜[\ÈˆÂˆ\]HˆÂˆœÙ\ÜÚ[Û•\]Hˆ˜YÙ[ÛY\ÜØYÙWØÚ[šÈ‹ˆ˜ÛÛ[ˆÈ^ˆšYÈBˆBˆBˆJKˆ
NÂˆš]ÛØœÙ\™\—Ù]™[İ×ØYÙ]
	›]]]™[
NÂ‚ˆ\ÜÙ\JÙ\šX[^™Y
	™]™[
K›[Š
HHĞ”ÑT•‘T—ÓPVÔRS•VÓSŠNÂˆ]^H]™[œ^[ØYÈœ\˜[\È—VÈ\]H—VÈ˜ÛÛ[—VÈ^—Bˆ˜\×ÜİŠ
Bˆ[Ü˜\

NÂˆ\ÜÙ\J^˜ÛÛZ[œÊ¸ )–Ù[YYŠK›™\İYXYˆØ\È[YYŠNÂˆB‚ˆÖİ\İBˆ›ˆ\İÛX[WÛYY][WÛX]™\×İ\›Z[˜]WİšXWÜİXŠ
HÂˆËÈX[HX]™\ÈXXÚÛÈÛX[ÈÚš[šÈÛˆZ\ˆİÛˆ
™[İÈ™]Z[ŠKˆËÈÛÛXİ]™[Hİ™\ˆHØ\ˆ›ÈXYˆØ[ˆİšXİHÚš[šËÛÈHš[[Y\‚ˆËÈ]\İ\›Z[˜]HšXHHİXˆ˜]\ˆ[ˆÛÜ›Ü™]™\‹‚ˆ]XYˆH›H‹œ™\X]
Ğ”ÑT•‘T—ÓPQ—Ô‘URS—Ğ–UTÊNÈËÈÚÜ\ˆ[ˆXY
İZ[8¡¤ˆØ[››İÚš[šÂˆ]][\Îˆ™XÏÙ\™WÚœÛÛ•˜[YOˆH
‹
Bˆ›X\
ßÙ\™WÚœÛÛ•˜[YN”İš[™ÊXY‹˜ÛÛ™J
JJBˆ˜ÛÛXİ

NÂˆ]]]]™[H]™[İÚ]Ü^[ØY
˜XÜÜ™XY‹Ù\™WÚœÛÛšœÛÛˆJÈš][\Èˆ][\ÈJJNÂˆ\ÜÙ\JˆÙ\šX[^™Y
	™]™[
K›[Š
HˆĞ”ÑT•‘T—ÓPVÔRS•VÓS‹ˆœ™XÛÛ™][Ûˆœ˜[YH\Èİ™\ˆHØ\‚ˆ
NÂ‚ˆš]ÛØœÙ\™\—Ù]™[İ×ØYÙ]
	›]]]™[
NÂ‚ˆ\ÜÙ\JÙ\šX[^™Y
	™]™[
K›[Š
HHĞ”ÑT•‘T—ÓPVÔRS•VÓSŠNÂˆ\ÜÙ\Ù\HJˆ]™[œ^[ØYÈ™[YY—K˜\×ÜİŠ
K[Ü˜\

Kˆ˜XÜÜ™XY^[ØYÛÈ\™ÙH‹ˆ™™[˜XÚÈÈHİXˆ‚ˆ
NÂˆ\ÜÙ\J]™[œ^[ØY™Ù]
›ÜšYÚ[˜[]\ÈŠKš\×ÜÛÛYJ
JNÂˆB‚ˆÖİ\İBˆ›ˆ\İÛXY—İÛ×ÜÛX[İ×ÜÚš[š×Ú\×Û›İÛ]]]Y

HÂˆËÈHœ˜[YH[™XYH[™\ˆYÙ]ÚÜÙHÛ›HXYˆ\È™[İÈHÚš[šÈ›ÛÜ‚ˆËÈ›İ[™ÈÚİ[Ú[™ÙKˆ
[™\‹XYÙ]ÚÜXÚ\˜İZ]Ë[™]™[ˆYˆ›Ü˜ÙYˆËÈXY—ÜÚš[šÜÈÛİ[™Z™Xİ]ŠBˆ]ÚÜHœÈ‹œ™\X]
Ğ”ÑT•‘T—ÓPQ—Ô‘URS—Ğ–UTÊNÈËÈOHXYÈØ[››İİšXİHÚš[šÂˆ\ÜÙ\Jˆ[XY—ÜÚš[šÜÊ	œÚÜ
Kˆ˜HXYˆ]H™]Z[ˆ›ÛÜˆ]\İ›İÚš[šÈ‚ˆ
NÂˆ]Û™Ù\ˆH“‹œ™\X]
Ğ”ÑT•‘T—ÓPQ—Ô‘URS—Ğ–UTÈ
ˆˆ
ÈL
NÂˆ\ÜÙ\JXY—ÜÚš[šÜÊ	›Û™Ù\ŠK˜HÛX\›H\™Ù\ˆXYˆ]\İÚš[šÈŠNÂˆB‚ˆÖİ\İBˆ›ˆ\İİ]Û][X]WÛXY—Ù[Y\×ÛÛ—ØÚ\—Ø›İ[™\J
HÂˆËÈHXYˆÙˆËX]HÚ\œÈ
8 )ˆHJÌŒŠH8 %[Y[™È]\İ[™ÛˆÚ\‚ˆËÈ›İ[™\šY\È[™™]™\ˆ[šXÈÜˆ›ÙXÙH[˜[YU‹N‚ˆ]šYÎˆİš[™ÈH¸ )ˆ‹œ™\X]
Ì
NÈËÈLŒÌ]\Âˆ]]]]™[H]™[İÚ]Ü^[ØY
˜XÜÜ™XY‹Ù\™WÚœÛÛšœÛÛˆJÈ˜›ÙHˆšYÈJJNÂˆš]ÛØœÙ\™\—Ù]™[İ×ØYÙ]
	›]]]™[
NÂ‚ˆ\ÜÙ\JÙ\šX[^™Y
	™]™[
K›[Š
HHĞ”ÑT•‘T—ÓPVÔRS•VÓSŠNÂˆ]XYˆH]™[œ^[ØYÈ˜›ÙH—K˜\×ÜİŠ
K[Ü˜\

NÂˆËÈ˜[YU‹NHÛÛœİXİ[Ûˆ
]	ÜÈH	œİŠNÈÛÛ™š\›HXYİZ[\™HÚÛBˆËÈ][KX]HÚ\œÈ[™HX\šÙ\ˆ\È™\Ù[‚ˆ\ÜÙ\JXY‹œİ\×İÚ]
	ø )‰ÊJNÂˆ\ÜÙ\JXY‹™[™×İÚ]
	ø )‰ÊJNÂˆ\ÜÙ\JXY‹˜ÛÛZ[œÊ–Ù[YYŠJNÂˆBŸB