//! ACP client module â€” manages communication with an AI agent subprocess over stdio
//! using JSON-RPC 2.0 (newline-delimited / NDJSON).
//!
//! # Lifecycle
//! 1. [`AcpClient::spawn`] â€” launch agent binary as subprocess
//! 2. [`AcpClient::initialize`] â€” protocol version negotiation
//! 3. [`AcpClient::session_new`] â€” create session with MCP server config
//! 4. [`AcpClient::session_prompt_with_idle_timeout`] â€” send prompt with idle/hard deadline, return stop reason
//! 5. [`AcpClient::session_cancel`] / [`AcpClient::cancel_with_cleanup`] â€” cancel in-flight turn

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio_util::codec::{FramedRead, LinesCodec, LinesCodecError};

use crate::observer::{ObserverContext, ObserverHandle};
use crate::permission::{
    OwnerPermissionDecision, PermissionBinding, PermissionWaitOutcome, PERMISSION_DECISION_TIMEOUT,
};
use crate::usage::{TurnUsage, UsageTracker};

/// Maximum allowed size of a single NDJSON line from the agent's stdout.
/// Lines exceeding this limit are rejected to prevent OOM from rogue agents.
const MAX_LINE_SIZE: usize = 10_000_000; // 10 MB

/// An MCP server configuration passed to `session/new`.
///
/// Corresponds to the `McpServerStdio` variant in the ACP schema.
/// All four fields are **required** by the schema (`args` and `env` may be empty arrays).
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<EnvVar>,
}

/// A single environment variable for an MCP server.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// Stop reason returned by `session/prompt` when the agent finishes a turn.
///
/// Maps to the `stopReason` field in the `SessionPromptResponse`.
#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    /// Agent completed the turn normally (`"end_turn"`).
    EndTurn,
    /// Turn was cancelled via `session/cancel` (`"cancelled"`).
    Cancelled,
    /// Agent hit its token limit (`"max_tokens"`).
    MaxTokens,
    /// Agent hit its per-turn request limit (`"max_turn_requests"`).
    MaxTurnRequests,
    /// Agent refused the prompt (`"refusal"`).
    /// Note: refused turns are dropped from history by the agent.
    Refusal,
}

impl StopReason {
    /// Parse a `stopReason` string from the ACP wire format.
    ///
    /// Matching is case-insensitive so agents that send `"END_TURN"` or
    /// `"Cancelled"` are handled correctly without a protocol error.
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "end_turn" => Some(Self::EndTurn),
            "cancelled" => Some(Self::Cancelled),
            "max_tokens" => Some(Self::MaxTokens),
            "max_turn_requests" => Some(Self::MaxTurnRequests),
            "refusal" => Some(Self::Refusal),
            _ => None,
        }
    }
}
/// Sealed terminal result of a matching `session/prompt` request.
///
/// The final reply is deliberately supplied only by the acknowledged
/// host-final extension on that request's result. It is never reconstructed
/// from streamed updates, tool arguments, thoughts, or permission data.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptCompletion {
    pub stop_reason: StopReason,
    pub final_reply: Option<String>,
}

impl PromptCompletion {
    pub fn is_publishable_terminal(&self) -> bool {
        matches!(self.stop_reason, StopReason::EndTurn | StopReason::Refusal)
    }
}

/// Errors that can occur in the ACP client.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// A structurally invalid host execution request rejected before any ACP
    /// method is sent to the agent runtime.
    #[error("Invalid execution request: {0}")]
    InvalidRequest(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Agent process exited unexpectedly")]
    AgentExited,

    #[error("Idle timeout â€” no agent activity for {0:?}")]
    IdleTimeout(std::time::Duration),

    #[error("Hard turn timeout exceeded (silence {silence:?})")]
    HardTimeout { silence: std::time::Duration },

    #[error("Agent did not stop within {0:?} after cancellation")]
    CancelDrainTimeout(std::time::Duration),

    #[error("Request timeout â€” agent did not respond within {0:?}")]
    Timeout(std::time::Duration),

    #[error("Write timeout â€” agent stopped reading stdin (blocked for {0:?})")]
    WriteTimeout(std::time::Duration),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Agent reported error (code {code}): {message}")]
    AgentError { code: i64, message: String },
}

/// Build an [`AcpError::AgentError`] from a JSON-RPC error object,
/// preserving the numeric code. When the `message` field is missing or
/// non-string, fall back to the full JSON object so provider-specific
/// detail (e.g. a `data` field) is not lost.
fn agent_error_from_json(error: &serde_json::Value) -> AcpError {
    let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-32000);
    let message = match error.get("message").and_then(|m| m.as_str()) {
        Some(m) => m.to_string(),
        None => error.to_string(),
    };
    AcpError::AgentError { code, message }
}

fn build_initialize_params() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": 2,
        "clientCapabilities": build_client_capabilities(),
        "clientInfo": {
            "name": "buzz-acp",
            "version": env!("CARGO_PKG_VERSION")
        },
    })
}

/// ACP client that owns an agent subprocess and communicates over its stdio.
///
/// One `AcpClient` per agent process. Multiple sessions can be created on the
/// same client via repeated calls to [`session_new`](AcpClient::session_new).
pub struct AcpClient {
    /// The agent child process (kept alive to prevent zombie).
    child: Child,
    /// Write end of the agent's stdin pipe.
    stdin: ChildStdin,
    /// Framed reader over the agent's stdout pipe (line-oriented, bounded).
    /// Uses `LinesCodec::new_with_max_length` to enforce MAX_LINE_SIZE at the
    /// read level â€” prevents OOM from rogue agents writing infinite non-newline bytes.
    reader: FramedRead<ChildStdout, LinesCodec>,
    /// Monotonically increasing JSON-RPC request id counter.
    /// Harness-generated IDs are always numeric.
    next_id: u64,
    /// The id of a `session/request_permission` request that has been received
    /// but not yet responded to. Stored as `serde_json::Value` because JSON-RPC 2.0
    /// permits both numeric and string IDs from the agent.
    /// Used by [`cancel_with_cleanup`](AcpClient::cancel_with_cleanup) to send
    /// a `cancelled` outcome before the agent returns from `session/prompt`.
    pending_permission_id: Option<serde_json::Value>,
    /// Whether we have already sent a response to the pending permission request.
    /// Guards against double-response if a timeout fires after the rejection
    /// response was written but before `pending_permission_id` was cleared.
    permission_responded: bool,
    /// Complete bridge binding for the pending permission request.
    pending_permission_binding: Option<PermissionBinding>,
    /// The JSON-RPC id of the most recently sent `session/prompt` request.
    /// Used by [`cancel_with_cleanup`] to drain the correct response.
    /// Set in [`session_prompt_with_idle_timeout`]; consumed in [`cancel_with_cleanup`].
    last_prompt_id: Option<u64>,
    /// Hard deadline for the current turn, set by `session_prompt_with_idle_timeout`.
    /// Inherited by `cancel_with_cleanup` so the drain loop shares the same budget
    /// rather than starting a fresh timer (prevents double-jeopardy).
    current_hard_deadline: Option<tokio::time::Instant>,
    /// Optional local observer feed used by the desktop app.
    observer: Option<ObserverHandle>,
    /// Pool slot index for this agent process.
    observer_agent_index: Option<usize>,
    /// Best-effort context attached to raw ACP wire events.
    observer_context: ObserverContext,
    /// Most recently observed `_meta.goose.activeRunId` from a
    /// `session/update` notification of kind `session_info_update`.
    ///
    /// Both goose and buzz-agent emit `session_info_update` with this field;
    /// goose emits it whenever it starts or clears an active prompt run
    /// (`crates/goose/src/acp/server.rs:2277` `send_active_run_update`).
    /// Required as `expectedRunId` when calling the non-standard
    /// `_goose/unstable/session/steer` method to inject a message into an
    /// in-flight turn without cancelling it.
    ///
    /// `None` until the first `session_info_update` arrives, or after the
    /// run clears (goose/buzz-agent emit `activeRunId: null` at end of turn).
    /// Other agents may leave this unset â€” readers must treat `None` as
    /// "no active run to steer into" and fall back to cancel+merge.
    active_run_id: Option<String>,
    /// Whether the agent advertised `_meta.steering.supported: true` in its
    /// `initialize` response, meaning it implements the cross-adapter
    /// [`ACP_STEER_METHOD`] extension.
    ///
    /// Set once by [`initialize`](Self::initialize); `false` for agents that
    /// omit the key. This is the **only** gate on writing an
    /// [`ACP_STEER_METHOD`] request. It must never be replaced by error-code
    /// probing: codex-acp answers unrecognized extension methods with `{}` â€”
    /// a JSON-RPC *success*, not `-32601` â€” which the main loop would read as
    /// a delivered steer and drop the user's message from the queue.
    steering_supported: bool,
    /// Per-turn channel for receiving goose-native non-cancelling steer
    /// requests from the main loop. Installed by
    /// [`install_steer_rx`](Self::install_steer_rx) at dispatch and
    /// consumed (via `take()`) by `session_prompt_with_idle_timeout` so it
    /// is dropped at scope exit alongside the turn it served. `None`
    /// outside of a goose-native turn â€” the read loop's steer arm is
    /// disabled in that case.
    steer_rx: Option<tokio::sync::mpsc::Receiver<crate::pool::SteerRequest>>,
    /// Usage tracker â€” accumulates cumulative token counts from
    /// `_goose/unstable/session/update` notifications and computes per-turn
    /// deltas. Both goose and buzz-agent emit this notification; goose gates
    /// on client capability advertisement, buzz-agent emits unconditionally.
    goose_usage: UsageTracker,
    /// Whether initialize acknowledged Pkzz's fixed-route host-final reply
    /// contract. This gate is independent from the owner-permission bridge.
    host_final_reply_supported: bool,
    /// The semantic completion for the most recently completed prompt. It is
    /// sealed before `last_prompt_id` is cleared, so a control signal that wins
    /// immediately afterward can recover the actual terminal result.
    sealed_prompt_completion: Option<PromptCompletion>,
}

/// Recursively merge `overlay` into `base`, with `overlay` winning on scalar/shape
/// collisions.  When both sides have an object for the same key, the merge recurses so
/// unrelated nested keys from `base` are preserved.
fn deep_merge(
    base: &mut serde_json::Map<String, serde_json::Value>,
    overlay: serde_json::Map<String, serde_json::Value>,
) {
    for (k, overlay_val) in overlay {
        match base.get_mut(&k) {
            Some(serde_json::Value::Object(base_obj))
                if matches!(overlay_val, serde_json::Value::Object(_)) =>
            {
                // Both sides are objects â€” recurse to preserve unrelated nested keys.
                if let serde_json::Value::Object(overlay_obj) = overlay_val {
                    deep_merge(base_obj, overlay_obj);
                }
            }
            _ => {
                // Scalar, array, type mismatch, or new key â€” overlay wins.
                base.insert(k, overlay_val);
            }
        }
    }
}

/// Build the merged `CODEX_CONFIG` environment-variable value for a Codex agent spawn.
///
/// Returns `Some(json_string)` when `has_generated_codex_config` is true (Pkzz injected a
/// `CODEX_CONFIG` entry via `codex_network_env()`), `None` otherwise.
///
/// # Merge contract (when `has_generated_codex_config` is true)
///
/// 1. **Persona base** â€” the first `CODEX_CONFIG` value in `extra_env` is taken as
///    the base object (all keys preserved, recursively).  When there is no persona entry,
///    the generated entry serves as the base.
/// 2. **Generated overlay** â€” all subsequent `CODEX_CONFIG` entries are deep-merged into
///    the base so unrelated nested persona keys survive.
/// 3. **Parent-env precedence** â€” if `parent_codex_config` is `Some`, its keys are
///    deep-merged into the result (parent wins on colliding keys at every nesting level;
///    unrelated keys from either side survive).
/// 4. **Forced overlay** â€” `sandbox_workspace_write.network_access = true` is applied
///    last so relay access is guaranteed regardless of operator / persona config.
///
/// When `has_generated_codex_config` is false, the function returns `None` and the
/// caller handles any persona-supplied `CODEX_CONFIG` with ordinary operator-wins
/// semantics (no merging, no sandbox widening).
///
/// # Errors
///
/// Returns `Err(AcpError::Protocol)` when `has_generated_codex_config` is true and any
/// `CODEX_CONFIG` value is not valid JSON or is not a JSON object, or when
/// `sandbox_workspace_write` is present but not an object after all merges.
pub(crate) fn build_codex_config_env(
    extra_env: &[(String, String)],
    parent_codex_config: Option<&str>,
    has_generated_codex_config: bool,
) -> Result<Option<String>, AcpError> {
    // Without an explicit Pkzz-generated overlay signal, skip the merge entirely.
    // Any persona CODEX_CONFIG is handled by the caller with operator-wins semantics.
    if !has_generated_codex_config {
        return Ok(None);
    }

    // Collect all CODEX_CONFIG entries from extra_env in order.
    let codex_entries: Vec<&str> = extra_env
        .iter()
        .filter(|(k, _)| k == "CODEX_CONFIG")
        .map(|(_, v)| v.as_str())
        .collect();

    if codex_entries.is_empty() {
        // has_generated_codex_config is true but no entry in extra_env â€” shouldn't
        // happen in practice, but treat as no-op rather than panic.
        return Ok(None);
    }

    // Parse all entries; first one is the persona base (or the generated entry if no
    // persona CODEX_CONFIG was set), rest are additional generated entries.
    let mut parsed_entries: Vec<serde_json::Map<String, serde_json::Value>> = Vec::new();
    for (i, raw) in codex_entries.iter().enumerate() {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(serdëÍ¸îÚ$z{-®éÜj×öåWFFR#¢'W6vU÷WFFR"À¢'W6VB#¢–çWB²÷WGWBÀ¢&6öçFW‡DÆ–Ö—B#¢#ScBÀ¢&67V×VÆFVD–çWEFö¶Vç2#¢–çWBÀ¢&67V×VÆFVD÷WGWEFö¶Vç2#¢÷WGWBÀ¢Ò“°¢–bÆWB6öÖR†2’Ò6÷7B°¢WFFU²&67V×VÆFVD6÷7B%ÒÒ6W&FUö§6öã£¦§6öâ†2“°¢Ğ¢6W&FUö§6öã£¦§6öâ‡°¢&§6öç'2#¢#"ã"À¢&ÖWF†öB#¢%övö÷6R÷Vç7F&ÆR÷6W76–öâ÷WFFR"À¢'&×2#¢°¢'6W76–öä–B#¢6W76–öåö–BÀ¢'WFFR#¢WFFP¢Ğ¢Ò¢Ğ ¢5·Fö¶–ó£§FW7EĞ¢7–æ2fâvö÷6U÷W6vUöæ÷F–f–6F–öå÷&V6÷&FVEöæE÷F¶U÷&WGW&ç5÷W6vR‚’°¢ÆWB×WB6Æ–VçBÒ7våö–æW'Eö6Æ–VçB‚’æv—C°¢76W'B†6Æ–VçBçF¶U÷GW&å÷W6vR‚’æ—5öæöæR‚’Â'7F'G2V×G’"“° ¢òò&Vv–å÷GW&â&Vf÷&R6VæF–ærF†R&ö×B(	BÖ—'&÷'2F†R&VÂ6ÆÂfÆ÷rà¢6Æ–VçBævö÷6U÷W6vRæ&Vv–å÷GW&â‚'3"“°¢ÆWB×6rÒvö÷6U÷W6vU÷WFFUö×6r‚'3"ÂÂ#Â6öÖRƒã’“°¢6Æ–VçBæ†æFÆUövö÷6U÷W6vU÷WFFR‚f×6r“° ¢ÆWBW6vRÒ6Æ–Vç@¢çF¶U÷GW&å÷W6vR‚¢æW‡V7B‚'W6vR6†÷VÆB&R&W6VçBgFW"æ÷F–f–6F–öâ"“°¢76W'EöW‡W6vRç6W76–öåö–BÂ'3"“°¢76W'EöW‡W6vRçGW&å÷6WÂ“°¢76W'B‚W6vRæFVÇF÷&VÆ–&ÆRÂ&f—'7BGW&â×W7B&RVç&VÆ–&ÆR"“°¢76W'EöW‡W6vRæ7V×VÆF—fUö–çWE÷Fö¶Vç2Â“°¢76W'EöW‡W6vRæ7V×VÆF—fUö÷WGWE÷Fö¶Vç2Â#“°¢76W'EöW‡W6vRæ7V×VÆF—fUö6÷7E÷W6BÂ6öÖRƒã’“° ¢òò6V6öæBF¶R×W7B&RæöæRà¢76W'B€¢6Æ–VçBçF¶U÷GW&å÷W6vR‚’æ—5öæöæR‚’À¢'F¶RgFW"G&–â—2æöæR ¢“°¢Ğ ¢5·Fö¶–ó£§FW7EĞ¢7–æ2fâvö÷6U÷W6vU÷6V6öæE÷GW&åöFVÇF÷&VÆ–&ÆR‚’°¢ÆWB×WB6Æ–VçBÒ7våö–æW'Eö6Æ–VçB‚’æv—C°¢òòGW&âà¢6Æ–VçBævö÷6U÷W6vRæ&Vv–å÷GW&â‚'3""“°¢6Æ–VçBæ†æFÆUövö÷6U÷W6vU÷WFFR‚fvö÷6U÷W6vU÷WFFUö×6r‚'3""ÂÂ#ÂæöæR’“°¢ÆWBòÒ6Æ–VçBçF¶U÷GW&å÷W6vR‚“°¢òòGW&â"à¢6Æ–VçBævö÷6U÷W6vRæ&Vv–å÷GW&â‚'3""“°¢6Æ–VçBæ†æFÆUövö÷6U÷W6vU÷WFFR‚fvö÷6U÷W6vU÷WFFUö×6r‚'3""ÂƒÂCSÂæöæR’“°¢ÆWBW6vRÒ6Æ–VçBçF¶U÷GW&å÷W6vR‚’æW‡V7B‚'GW&â"W6vR"“°¢76W'B‡W6vRæFVÇF÷&VÆ–&ÆR“°¢76W'EöW‡W6vRçGW&åö–çWE÷Fö¶Vç2Â6öÖRƒƒ’“°¢76W'EöW‡W6vRçGW&åö÷WGWE÷Fö¶Vç2Â6öÖRƒ#S’“°¢Ğ ¢5·Fö¶–ó£§FW7EĞ¢7–æ2fâvö÷6U÷W6vUöÖÆf÷&ÖVEöæ÷F–f–6F–öåöFöW5öæ÷E÷æ–2‚’°¢ÆWB×WB6Æ–VçBÒ7våö–æW'Eö6Æ–VçB‚’æv—C°¢òòÖ—76–ær&×2VçF—&VÇ’à¢ÆWB&BÒ6W&FUö§6öã£¦§6öâ‡²&§6öç'2#¢#"ã"Â&ÖWF†öB#¢%övö÷6R÷Vç7F&ÆR÷6W76–öâ÷WFFR'Ò“°¢6Æ–VçBæ†æFÆUövö÷6U÷W6vU÷WFFR‚f&B“°¢76W'B†6Æ–VçBçF¶U÷GW&å÷W6vR‚’æ—5öæöæR‚’“° ¢òò&×2&W6VçB'WBw&öær6†Rà¢ÆWB&C"Ò6W&FUö§6öã£¦§6öâ‡°¢&§6öç'2#¢#"ã"À¢&ÖWF†öB#¢%övö÷6R÷Vç7F&ÆR÷6W76–öâ÷WFFR"À¢'&×2#¢²&ö÷2#¢G'VRĞ¢Ò“°¢6Æ–VçBæ†æFÆUövö÷6U÷W6vU÷WFFR‚f&C"“°¢76W'B†6Æ–VçBçF¶U÷GW&å÷W6vR‚’æ—5öæöæR‚’“°¢Ğ ¢5·FW7EĞ¢fâvVçEöW'&÷%ög&öÕö§6öåöfÆÇ5ö&6µ÷FõögVÆÅö§6öå÷v†VåöÖW76vUöÖ—76–ær‚’°¢òòW'&÷'2v—F†÷WB7G&–ærÖW76vVf–VÆB†RærâöæÇ’FFf–VÆB’×W7@¢òòæ÷B&R6–ÆVçFÇ’G'Væ6FVBFò'Væ¶æ÷vâW'&÷""(	BF†RgVÆÂ¥4ôâ—2&W6W'fVBà¢ÆWBW'&÷"Ò6W&FUö§6öã£¦§6öâ‡²&6öFR#¢Ó3#Â&FF#¢'V÷FW†6VVFVB'Ò“°¢ÖF6‚7WW#£¦vVçEöW'&÷%ög&öÕö§6öâ‚fW'&÷"’°¢7W'&÷#£¤vVçDW'&÷"²6öFRÂÖW76vRÒÓâ°¢76W'EöW†6öFRÂÓ3#“°¢76W'B€¢ÖW76vRæ6öçF–ç2‚'V÷FW†6VVFVB"’À¢&W‡V7FVBgVÆÂ¥4ôâ–âÖW76vRÂv÷C¢¶ÖW76vWÒ ¢“°¢Ğ¢÷F†W"Óâæ–2‚&W‡V7FVBvVçDW'&÷"Âv÷B¶÷F†W#£÷Ò"’À¢Ğ¢Ğ ¢5·FW7EĞ¢fâvVçEöW'&÷%ög&öÕö§6öå÷W6W5öÖW76vUöf–VÆE÷v†Vå÷&W6VçB‚’°¢ÆWBW'&÷"Ò6W&FUö§6öã£¦§6öâ‡²&6öFR#¢Ó3#Â&ÖW76vR#¢&WF‚FVæ–VB'Ò“°¢ÖF6‚7WW#£¦vVçEöW'&÷%ög&öÕö§6öâ‚fW'&÷"’°¢7W'&÷#£¤vVçDW'&÷"²6öFRÂÖW76vRÒÓâ°¢76W'EöW†6öFRÂÓ3#“°¢76W'EöW†ÖW76vRÂ&WF‚FVæ–VB"“°¢Ğ¢÷F†W"Óâæ–2‚&W‡V7FVBvVçDW'&÷"Âv÷B¶÷F†W#£÷Ò"’À¢Ğ¢Ğ ¢òò)H)H'V–ÆEö6öFW…ö6öæf–uöVçb)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H  ¢fâVçb‡—'3¢e²‚g7G"Âg7G"•Ò’ÓâfV3Â…7G&–ærÂ7G&–ær“â°¢—'0¢æ—FW"‚¢æÖ‡Â†²Âb—Â†²çFõ÷7G&–ær‚’ÂbçFõ÷7G&–ær‚’’¢æ6öÆÆV7B‚¢Ğ ¢6öç7BtTäU$DTC¢g7G"Ò"2'²'6æF&÷…÷v÷&·76U÷w&—FR#§²&æWGv÷&µö66W72#§G'VW×Ò"3° ¢5·FW7EĞ¢fâ'V–ÆEö6öFW…ö6öæf–uöVçe÷&WGW&ç5öæöæU÷v†Våöæõö6öFW…ö6öæf–uö–åöW‡G&öVçb‚’°¢òòæöâÔ6öFW‚vVçG3¢W‡G&öVçb†2æò4ôDU…ô4ôäd”r(i"æöæR&Vv&FÆW72öb6–væÂà¢ÆWBW‡G&ÒVçb‚e²‚$tôõ4Uõ$õd”DU""Â&÷Væ’"•Ò“°¢ÆWB&W7VÇBÒ'V–ÆEö6öFW…ö6öæf–uöVçb‚fW‡G&ÂæöæRÂfÇ6R’çVçw&‚“°¢76W'EöW€¢&W7VÇBÂæöæRÀ¢&æò4ôDU…ô4ôäd”r–âW‡G&öVçb×W7B&WGW&âæöæR ¢“°¢Ğ ¢5·FW7EĞ¢fâ'V–ÆEö6öFW…ö6öæf–uöVçeövVæW&FVEööæÇ•÷6–ævÆUöVçG'•÷v—F…÷6–væÅ÷G'VUöÖW&vW5÷v—F…÷&VçB‚’°¢òòæòW'6öæ¢·§¢–æ¦V7G2öæR4ôDU…ô4ôäd”s²6–væÃ×G'VRà¢òò&VçBÖ’†fR—G2÷vâ4ôDU…ô4ôäd”r(	BFVWöÖW&vRÆ–W2ÂæWGv÷&µö66W72f÷&6VBà¢ÆWBW‡G&ÒVçb‚e²‚$4ôDU…ô4ôäd”r"ÂtTäU$DTB•Ò“°¢ÆWB&VçBĞ¢"2'²'6öÖUö÷W&F÷%ö¶W’#¢'fÂ"Â'6æF&÷…÷v÷&·76U÷w&—FR#§²&÷W&F÷%ö¶W’#¢&¶VW'×Ò"3°¢ÆWBÖW&vVBÒ'V–ÆEö6öFW…ö6öæf–uöVçb‚fW‡G&Â6öÖR‡&VçB’ÂG'VR¢çVçw&‚¢çVçw&‚“°¢ÆWBc¢6W&FUö§6öã£¥fÇVRÒ6W&FUö§6öã£¦g&öÕ÷7G"‚fÖW&vVB’çVçw&‚“°¢òòæWGv÷&µö66W72f÷&6VBG'VRWfVâF†÷Vv‚öæÇ’öæRVçG'’–âW‡G&öVçbà¢76W'EöW€¢e²'6æF&÷…÷v÷&·76U÷w&—FR%Õ²&æWGv÷&µö66W72%ÒÂG'VRÀ¢&æWGv÷&µö66W72×W7B&Rf÷&6VBG'VRv—F‚6–væÃ×G'VR ¢“°¢òò÷W&F÷"¶W’&W6W'fVBf–FVWöÖW&vRà¢76W'EöW€¢e²'6æF&÷…÷v÷&·76U÷w&—FR%Õ²&÷W&F÷%ö¶W’%ÒÂ&¶VW"À¢&÷W&F÷"æW7FVB¶W’×W7B7W'f—fR ¢“°¢76W'EöW€¢e²'6öÖUö÷W&F÷%ö¶W’%ÒÂ'fÂ"À¢&÷W&F÷"F÷ÖÆWfVÂ¶W’×W7B7W'f—fR ¢“°¢Ğ ¢5·FW7EĞ¢fâ'V–ÆEö6öFW…ö6öæf–uöVçe÷W'6öæööæÇ•÷6–væÅöfÇ6U÷&WGW&ç5öæöæR‚’°¢òòW'6öæ6WB4ôDU…ô4ôäd”s²·§¢F–Bæ÷B–æ¦V7BvVæW&FVB÷fW&Æ’‡6–væÃÖfÇ6R’à¢òò×W7B&WGW&âæöæR(	BæòÖW&v–ærÂæò6æF&÷‚v–FVæ–ærà¢ÆWBW'6öæÒ"2'²'6öÖUöfVGW&R#¢&öâ'Ò"3°¢ÆWBW‡G&ÒVçb‚e²‚$4ôDU…ô4ôäd”r"ÂW'6öæ•Ò“°¢ÆWB&W7VÇBÒ'V–ÆEö6öFW…ö6öæf–uöVçb‚fW‡G&ÂæöæRÂfÇ6R’çVçw&‚“°¢76W'EöW€¢&W7VÇBÂæöæRÀ¢'W'6öæÖöæÇ’4ôDU…ô4ôäd”rv—F‚6–væÃÖfÇ6R×W7B&WGW&âæöæR ¢“°¢Ğ ¢5·FW7EĞ¢fâ'V–ÆEö6öFW…ö6öæf–uöVçe÷&WGW&ç5öæöæUöf÷%÷W'6öæööæÇ•öæõövVæW&FVEö÷fW&Æ’‚’°¢òòÆ–3¢6ÖR66Væ&–ò2&÷fRÂ6öæf—&×2F†RöÆB6÷VçBÖ&6VBF‚æòÆöævW"W†—7G2à¢ÆWBW'6öæÒ"2'²'6öÖUöfVGW&R#¢&öâ'Ò"3°¢ÆWBW‡G&ÒVçb‚e²‚$4ôDU…ô4ôäd”r"ÂW'6öæ•Ò“°¢ÆWB&W7VÇBÒ'V–ÆEö6öFW…ö6öæf–uöVçb‚fW‡G&ÂæöæRÂfÇ6R’çVçw&‚“°¢76W'EöW€¢&W7VÇBÂæöæRÀ¢'W'6öæÖöæÇ’4ôDU…ô4ôäd”rv—F‚6–væÃÖfÇ6R×W7B&WGW&âæöæR ¢“°¢Ğ ¢5·FW7EĞ¢fâ'V–ÆEö6öFW…ö6öæf–uöVçe÷6WG5öæWGv÷&µö66W75ög&öÕ÷67&F6‚‚’°¢òòW'6öæ²vVæW&FVB÷fW&Æ’Â6–væÃ×G'VS¢æWGv÷&µö66W72—2f÷&6VBG'VRà¢ÆWBW'6öæÒ"2'·Ò"3°¢ÆWBW‡G&ÒVçb‚e²‚$4ôDU…ô4ôäd”r"ÂW'6öæ’Â‚$4ôDU…ô4ôäd”r"ÂtTäU$DTB•Ò“°¢ÆWBÖW&vVBÒ'V–ÆEö6öFW…ö6öæf–uöVçb‚fW‡G&ÂæöæRÂG'VR’çVçw&‚’çVçw&‚“°¢ÆWBc¢6W&FUö§6öã£¥fÇVRÒ6W&FUö§6öã£¦g&öÕ÷7G"‚fÖW&vVB’çVçw&‚“°¢76W'EöW‡e²'6æF&÷…÷v÷&·76U÷w&—FR%Õ²&æWGv÷&µö66W72%ÒÂG'VR“°¢Ğ ¢5·FW7EĞ¢fâ'V–ÆEö6öFW…ö6öæf–uöVçe÷W'6öæö¶W—5÷7W'f—fUöÖW&vR‚’°¢òòW'6öæ†24ôDU…ô4ôäd”rv—F‚Vç&VÆFVB¶W—3²vVæW&FVB÷fW&Æ’×W7@¢òòf÷&6RæWGv÷&µö66W73×G'VRv—F†÷WBW&6–ærW'6öæ¶W—2à¢ÆWBW'6öæö6frÒ"2'²'6öÖUöfVGW&R#§²&Væ&ÆVB#§G'VW×Ò"3°¢òò6öæf–s£¦g&öÕö&w2VæG2vVæW&FVBeDU"W'6öæVçbf'2à¢ÆWBW‡G&ÒVçb‚e²‚$4ôDU…ô4ôäd”r"ÂW'6öæö6fr’Â‚$4ôDU…ô4ôäd”r"ÂtTäU$DTB•Ò“°¢ÆWBÖW&vVBÒ'V–ÆEö6öFW…ö6öæf–uöVçb‚fW‡G&ÂæöæRÂG'VR’çVçw&‚’çVçw&‚“°¢ÆWBc¢6W&FUö§6öã£¥fÇVRÒ6W&FUö§6öã£¦g&öÕ÷7G"‚fÖW&vVB’çVçw&‚“°¢76W'EöW€¢e²'6öÖUöfVGW&R%Õ²&Væ&ÆVB%ÒÂG'VRÀ¢'W'6öæ¶W’×W7B7W'f—fRÖW&vR ¢“°¢76W'EöW€¢e²'6æF&÷…÷v÷&·76U÷w&—FR%Õ²&æWGv÷&µö66W72%ÒÂG'VRÀ¢&æWGv÷&µö66W72×W7B&Rf÷&6VBG'VR ¢“°¢Ğ ¢5·FW7EĞ¢fâ'V–ÆEö6öFW…ö6öæf–uöVçeöæW7FVE÷W'6öæö¶W—5÷7W'f—fU÷v†Vå÷&VçEö†5÷6ÖU÷F÷öÆWfVÅö¶W’‚’°¢òòW'6öæ†26æF&÷…÷v÷&·76U÷w&—FRçW'6öæööæÇ“²&VçB†0¢òò6æF&÷…÷v÷&·76U÷w&—FRç&VçEööæÇ’âfÆBF÷ÖÆWfVÂ7&VBv÷VÆBG&÷ ¢òòW'6öæööæÇ’âFVWöÖW&vR×W7B&W6W'fR&÷F‚æW7FVB¶W—2Âæ@¢òòæWGv÷&µö66W72×W7B&Rf÷&6VBG'VRÆ7Bà¢ÆWBW'6öæö6frÒ"2'²'6æF&÷…÷v÷&·76U÷w&—FR#§²'W'6öæööæÇ’#¢&¶VWöÖR'×Ò"3°¢ÆWBW‡G&ÒVçb‚e²‚$4ôDU…ô4ôäd”r"ÂW'6öæö6fr’Â‚$4ôDU…ô4ôäd”r"ÂtTäU$DTB•Ò“°¢ÆWB&VçBÒ"2'²'6æF&÷…÷v÷&·76U÷w&—FR#§²'&VçEööæÇ’#¢&Ç6õö†W&R'×Ò"3°¢ÆWBÖW&vVBÒ'V–ÆEö6öFW…ö6öæf–uöVçb‚fW‡G&Â6öÖR‡&VçB’ÂG'VR¢çVçw&‚¢çVçw&‚“°¢ÆWBc¢6W&FUö§6öã£¥fÇVRÒ6W&FUö§6öã£¦g&öÕ÷7G"‚fÖW&vVB’çVçw&‚“°¢òò&÷F‚æW7FVB¶W—27W'f—fR(	BæòfÆB×7&VBG&÷à¢76W'EöW€¢e²'6æF&÷…÷v÷&·76U÷w&—FR%Õ²'W'6öæööæÇ’%ÒÂ&¶VWöÖR"À¢&æW7FVBW'6öæ¶W’×W7B7W'f—fRv†Vâ&VçB†2F†R6ÖRF÷ÖÆWfVÂ¶W’ ¢“°¢76W'EöW€¢e²'6æF&÷…÷v÷&·76U÷w&—FR%Õ²'&VçEööæÇ’%ÒÂ&Ç6õö†W&R"À¢&æW7FVB&VçB¶W’×W7B&R&W6VçB ¢“°¢òòf÷&6VBÆ7Bà¢76W'EöW€¢e²'6æF&÷…÷v÷&·76U÷w&—FR%Õ²&æWGv÷&µö66W72%ÒÂG'VRÀ¢&æWGv÷&µö66W72×W7B&Rf÷&6VBG'VR ¢“°¢Ğ ¢5·FW7EĞ¢fâ'V–ÆEö6öFW…ö6öæf–uöVçe÷&VçEöVçe÷v–ç5ööåö6öÆÆ—6–öç5÷W'6öæö¶W—5÷7W'f—fR‚’°¢òò&VçBVçb†24ôDU…ô4ôäd”rv—F‚6öÖR¶W—3²W'6öæ†2F–ffW&VçB¶W—2à¢òò&VçBv–ç2öâ6öÆÆ—6–öã²Vç&VÆFVBW'6öæ¶W—27W'f—fRà¢òòæWGv÷&µö66W72—2Çv—2f÷&6VBG'VRà¢ÆWBW'6öæö6frÒ"2'²'W'6öæö¶W’#¢'W'6öæ÷fÂ"Â'6†&VEö¶W’#¢'W'6öæ÷fW'6–öâ'Ò"3°¢òò6öæf–s£¦g&öÕö&w2VæG2vVæW&FVBeDU"W'6öæVçbf'2à¢ÆWBW‡G&ÒVçb‚e²‚$4ôDU…ô4ôäd”r"ÂW'6öæö6fr’Â‚$4ôDU…ô4ôäd”r"ÂtTäU$DTB•Ò“°¢ÆWB&VçBÒ"2'²'&VçEö¶W’#¢'&VçE÷fÂ"Â'6†&VEö¶W’#¢'&VçE÷fW'6–öâ'Ò"3°¢ÆWBÖW&vVBÒ'V–ÆEö6öFW…ö6öæf–uöVçb‚fW‡G&Â6öÖR‡&VçB’ÂG'VR¢çVçw&‚¢çVçw&‚“°¢ÆWBc¢6W&FUö§6öã£¥fÇVRÒ6W&FUö§6öã£¦g&öÕ÷7G"‚fÖW&vVB’çVçw&‚“°¢òò&VçBÖöæÇ’¶W’&W6Vç@¢76W'EöW€¢e²'&VçEö¶W’%ÒÂ'&VçE÷fÂ"À¢'&VçBÖöæÇ’¶W’×W7B&R&W6VçB ¢“°¢òòVç&VÆFVBW'6öæ¶W’7W'f—fW2†æò6öÆÆ—6–öâv—F‚&VçB¢76W'EöW€¢e²'W'6öæö¶W’%ÒÂ'W'6öæ÷fÂ"À¢'Vç&VÆFVBW'6öæ¶W’×W7B7W'f—fR ¢“°¢òò6öÆÆ—6–öã¢&VçBv–ç0¢76W'EöW€¢e²'6†&VEö¶W’%ÒÂ'&VçE÷fW'6–öâ"À¢'&VçB×W7Bv–âöâ6öÆÆ–F–ær¶W’ ¢“°¢òòæWGv÷&µö66W72Çv—2G'VR†f÷&6VBÆ7B¢76W'EöW‡e²'6æF&÷…÷v÷&·76U÷w&—FR%Õ²&æWGv÷&µö66W72%ÒÂG'VR“°¢Ğ ¢5·FW7EĞ¢fâ'V–ÆEö6öFW…ö6öæf–uöVçe÷&VçEö†5öW†—7F–æu÷6æF&÷…ö÷F†W%ö¶W—5÷7W'f—fR‚’°¢òò&VçBVçb†26æF&÷…÷v÷&·76U÷w&—FRv—F‚W‡G&¶W—3²gFW"ÖW&vP¢òòF†÷6RW‡G&¶W—27W'f—fRÆöæw6–FRæWGv÷&µö66W73×G'VRà¢ÆWBW'6öæÒ"2'·Ò"3°¢ÆWBW‡G&ÒVçb‚e²‚$4ôDU…ô4ôäd”r"ÂW'6öæ’Â‚$4ôDU…ô4ôäd”r"ÂtTäU$DTB•Ò“°¢ÆWB&VçBĞ¢"2'²'6æF&÷…÷v÷&·76U÷w&—FR#§²&æWGv÷&µö66W72#¦fÇ6RÂ&÷F†W%÷6æF&÷…ö¶W’#¢'fÂ'×Ò"3°¢ÆWBÖW&vVBÒ'V–ÆEö6öFW…ö6öæf–uöVçb‚fW‡G&Â6öÖR‡&VçB’ÂG'VR¢çVçw&‚¢çVçw&‚“°¢ÆWBc¢6W&FUö§6öã£¥fÇVRÒ6W&FUö§6öã£¦g&öÕ÷7G"‚fÖW&vVB’çVçw&‚“°¢òòæWGv÷&µö66W72f÷&6VBG'VRWfVâF†÷Vv‚&VçB6WBfÇ6P¢76W'EöW‡e²'6æF&÷…÷v÷&·76U÷w&—FR%Õ²&æWGv÷&µö66W72%ÒÂG'VR“°¢òò÷F†W%÷6æF&÷…ö¶W’7W'f—fW2‡&VçBw27w2ÖW&vVBÂF†VâæWGv÷&µö66W72f÷&6VB¢76W'EöW‡e²'6æF&÷…÷v÷&·76U÷w&—FR%Õ²&÷F†W%÷6æF&÷…ö¶W’%ÒÂ'fÂ"“°¢Ğ ¢5·FW7EĞ¢fâ'V–ÆEö6öFW…ö6öæf–uöVçeöW'&÷'5ööåö–çfÆ–E÷W'6öæö§6öâ‚’°¢òò&BW'6öæ¥4ôâ²vVæW&FVB÷fW&Æ’Â6–væÃ×G'VR(i"'6RW'&÷"&Vf÷&RÖW&v–ærà¢ÆWBW‡G&ÒVçb‚e²‚$4ôDU…ô4ôäd”r"Â&æ÷BÖ§6öâ"’Â‚$4ôDU…ô4ôäd”r"ÂtTäU$DTB•Ò“°¢ÆWB&W7VÇBÒ'V–ÆEö6öFW…ö6öæf–uöVçb‚fW‡G&ÂæöæRÂG'VR“°¢76W'B‡&W7VÇBæ—5öW'"‚’Â&–çfÆ–BW'6öæ¥4ôâ×W7B&WGW&âW'""“°¢ÆWB×6rÒf÷&ÖB‚'·Ò"Â&W7VÇBçVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚$4ôDU…ô4ôäd”r"’À¢&W'&÷"×W7BÖVçF–öâ4ôDU…ô4ôäd”r ¢“°¢Ğ ¢5·FW7EĞ¢fâ'V–ÆEö6öFW…ö6öæf–uöVçeöW'&÷'5ööåöæöåöö&¦V7E÷W'6öæö§6öâ‚’°¢òòæöâÖö&¦V7BW'6öæ¥4ôâ²vVæW&FVB÷fW&Æ’Â6–væÃ×G'VR(i"'6RW'&÷"à¢ÆWBW‡G&ÒVçb‚e²‚$4ôDU…ô4ôäd”r"Â%³Ã"Ã5Ò"’Â‚$4ôDU…ô4ôäd”r"ÂtTäU$DTB•Ò“°¢ÆWB&W7VÇBÒ'V–ÆEö6öFW…ö6öæf–uöVçb‚fW‡G&ÂæöæRÂG'VR“°¢76W'B‡&W7VÇBæ—5öW'"‚’Â&æöâÖö&¦V7BW'6öæ¥4ôâ×W7B&WGW&âW'""“°¢Ğ ¢5·FW7EĞ¢fâ'V–ÆEö6öFW…ö6öæf–uöVçeöW'&÷'5ööåö–çfÆ–E÷&VçEö§6öâ‚’°¢ÆWBW'6öæÒ"2'·Ò"3°¢ÆWBW‡G&ÒVçb‚e²‚$4ôDU…ô4ôäd”r"ÂW'6öæ’Â‚$4ôDU…ô4ôäd”r"ÂtTäU$DTB•Ò“°¢ÆWB&W7VÇBÒ'V–ÆEö6öFW…ö6öæf–uöVçb‚fW‡G&Â6öÖR‚&&BÖ§6öâ"’ÂG'VR“°¢76W'B‡&W7VÇBæ—5öW'"‚’Â&–çfÆ–B&VçBVçb¥4ôâ×W7B&WGW&âW'""“°¢Ğ ¢5·FW7EĞ¢fâ'V–ÆEö6öFW…ö6öæf–uöVçeöW'&÷'5ööåöæöåöö&¦V7E÷6æF&÷…÷v÷&·76U÷w&—FR‚’°¢òò6æF&÷…÷v÷&·76U÷w&—FR×W7B&Râö&¦V7Bf÷"æWGv÷&µö66W72f÷&6–ærà¢òò–bF†R&VçBVçb6WG2—BFòæöâÖö&¦V7B66Æ"ÂFVWöÖW&vR&WÆ6W0¢òò÷W"ö&¦V7Bv—F‚F†R66Æ"ÂæBF†Rf÷&6R7FW×W7Bf–Â6ÆV&Ç’à¢ÆWBW'6öæÒ"2'·Ò"3°¢ÆWBW‡G&ÒVçb‚e²‚$4ôDU…ô4ôäd”r"ÂW'6öæ’Â‚$4ôDU…ô4ôäd”r"ÂtTäU$DTB•Ò“°¢òò&VçB&WÆ6W2F†Rö&¦V7Bv—F‚66Æ"(	BFVWöÖW&vS¢66Æ"÷fW&Æ’v–ç2à¢ÆWB&VçBÒ"2'²'6æF&÷…÷v÷&·76U÷w&—FR#¢C'Ò"3°¢ÆWB&W7VÇBÒ'V–ÆEö6öFW…ö6öæf–uöVçb‚fW‡G&Â6öÖR‡&VçB’ÂG'VR“°¢76W'B€¢&W7VÇBæ—5öW'"‚’À¢&æöâÖö&¦V7B6æF&÷…÷v÷&·76U÷w&—FR×W7B&WGW&âW'" ¢“°¢ÆWB×6rÒf÷&ÖB‚'·Ò"Â&W7VÇBçVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚'6æF&÷…÷v÷&·76U÷w&—FR"’À¢&W'&÷"×W7BÖVçF–öâ6æF&÷…÷v÷&·76U÷w&—FR ¢“°¢Ğ§Ğ