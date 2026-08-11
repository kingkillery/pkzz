//! Agent pool â€” owns N AcpClient instances and dispatches prompt tasks.
//!
//! # Mental model
//!
//! ```text
//!   AgentPool
//!   â”œâ”€â”€ agents: Vec<Option<OwnedAgent>>   â† idle agents sit here
//!   â”œâ”€â”€ join_set: JoinSet<()>             â† in-flight tasks
//!   â”œâ”€â”€ task_map: HashMap<Id, TaskMeta>   â† panic recovery metadata
//!   â””â”€â”€ result_tx/rx: mpsc channel        â† tasks return agents here
//!
//!   Dispatch:
//!     try_claim() â†’ OwnedAgent (removed from slot)
//!     spawn run_prompt_task(agent, ...) into join_set
//!     task sends PromptResult { agent, outcome } via result_tx
//!     rx_and_join_set() â†’ poll result_rx for PromptResult
//!     return_agent(agent) â†’ puts agent back in slot
//! ```
//!
//! `AcpClient` is NOT Clone â€” ownership moves out on claim and back on return.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;
use uuid::Uuid;

use crate::acp::{
    extract_model_config_options, extract_model_state, model_in_catalog,
    resolve_model_switch_method, AcpClient, AcpError, EnvVar, McpServer, ModelSwitchMethod,
    PromptCompletion, StopReason, SystemPromptTransport,
};
use crate::config::{compose_session_title, DedupMode, PermissionMode};
use crate::final_delivery::{finalize_host_final_reply, HostFinalDeliveryOutbox};
use crate::observer;
use crate::ompk_execution::OmpkExecutionPolicy;
use crate::queue::{
    CancelReason, ContextMessage, ConversationContext, FinalReplyTarget, FlushBatch,
    PromptChannelInfo, PromptProfile, PromptProfileLookup, ThreadTags,
};
use crate::relay::{ChannelInfo, RestClient};

/// Window within which agent activity before a hard-cap death qualifies
/// the turn as "recently active" (eligible for requeue instead of dead-letter).
const RECENT_ACTIVITY_WINDOW: Duration = Duration::from_secs(60);

// FlushBatch and BatchEvent derive Clone (added in queue.rs) so we can store
// a recoverable copy in TaskMeta for panic recovery in Queue mode.

/// Metadata stored per in-flight task for panic recovery.
pub struct TaskMeta {
    pub agent_index: usize,
    pub channel_id: Option<Uuid>,
    /// Identifies terminal events when the task panics before returning a result.
    pub turn_id: String,
    /// Clone of batch for Queue mode panic recovery.
    pub recoverable_batch: Option<FlushBatch>,
    /// Control signal for the in-flight prompt task.
    /// `None` for heartbeat tasks (not controllable) and after signal is consumed.
    pub control_tx: Option<tokio::sync::oneshot::Sender<ControlSignal>>,
    /// Steer request channel for non-cancelling mid-turn delivery.
    /// Capacity-1; `try_send` from the main loop fails on `Full`/`Closed`,
    /// in which case the caller must fall back to the universal
    /// `ControlSignal::Steer` cancel+merge path. `None` for heartbeat
    /// tasks only â€” all prompt tasks install a steer channel regardless
    /// of the agent's name. The sender also owns the per-turn replay-fence
    /// accumulator shared with `run_prompt_task`.
    pub steer_tx: Option<SteerSender>,
}

/// Agent-level model capabilities. Populated on first session creation.
/// The catalog is the same across all sessions for a given agent process.
/// Fields are read by the desktop's `get_agent_models` Tauri command (Phase 3).
#[allow(dead_code)] // Scaffolding for desktop integration â€” fields read via serde.
pub struct AgentModelCapabilities {
    /// Stable: configOptions with category "model" from session/new.
    pub config_options_raw: Vec<serde_json::Value>,
    /// Unstable: SessionModelState from session/new.
    pub available_models_raw: Option<serde_json::Value>,
}

/// Per-channel session IDs and turn counters.
///
/// Separated from `OwnedAgent` so the state machine is testable without
/// spawning a real agent subprocess.
#[derive(Default)]
pub struct SessionState {
    /// channel_id â†’ session_id
    pub sessions: HashMap<Uuid, String>,
    /// channel_id â†’ cwd used for `session/new`.
    pub session_cwds: HashMap<Uuid, String>,
    pub heartbeat_session: Option<String>,
    /// Per-channel turn counters for proactive session rotation.
    /// Incremented on each successful prompt; reset when the session is rotated.
    pub turn_counts: HashMap<Uuid, u32>,
    /// Turn counter for the heartbeat session.
    pub heartbeat_turn_count: u32,
    /// channel_id â†’ rendered NIP-AE core prompt section, populated once at
    /// session creation per Tyler's spec (no mid-session refresh).
    pub core_sections: HashMap<Uuid, String>,
    /// channel_id â†’ rendered `[Channel Canvas]` metadata section.
    ///
    /// Populated once before session creation (same lifecycle as `core_sections`).
    /// Absent when the channel has no canvas, the canvas content is blank, or the
    /// fetch fails â€” all fail open. Cleared on session invalidation alongside
    /// `core_sections` so the next session picks up any canvas change.
    pub canvas_sections: HashMap<Uuid, String>,
}

impl SessionState {
    /// Invalidate the session (and turn counter) for a specific prompt source.
    pub fn invalidate(&mut self, source: &PromptSource) {
        match source {
            PromptSource::Channel(cid) => {
                self.invalidate_channel(cid);
            }
            PromptSource::Heartbeat => {
                self.heartbeat_session = None;
                self.heartbeat_turn_count = 0;
            }
        }
    }

    /// Invalidate a single channel's session and turn counter.
    /// Returns `true` if the channel had an active session.
    pub fn invalidate_channel(&mut self, channel_id: &Uuid) -> bool {
        self.turn_counts.remove(channel_id);
        self.core_sections.remove(channel_id);
        self.canvas_sections.remove(channel_id);
        self.session_cwds.remove(channel_id);
        self.sessions.remove(channel_id).is_some()
    }

    /// Rotate an existing channel session when its selected cwd changes.
    /// Missing cwd metadata is treated as unsafe to reuse.
    fn rotate_if_cwd_changed(&mut self, channel_id: &Uuid, desired_cwd: &str) -> bool {
        if !self.sessions.contains_key(channel_id) {
            return false;
        }
        if self.session_cwds.get(channel_id).map(String::as_str) == Some(desired_cwd) {
            return false;
        }
        self.invalidate_channel(channel_id);
        true
    }

    /// Invalidate all sessions and turn counters (e.g. after agent exit).
    pub fn invalidate_all(&mut self) {
        self.sessions.clear();
        self.session_cwds.clear();
        self.turn_counts.clear();
        self.heartbeat_session = None;
        self.heartbeat_turn_count = 0;
        self.core_sections.clear();
        self.canvas_sections.clear();
    }

    #[cfg(test)]
    fn has_channel_state(&self, channel_id: &Uuid) -> bool {
        self.sessions.contains_key(channel_id)
            || self.session_cwds.contains_key(channel_id)
            || self.turn_counts.contains_key(channel_id)
            || self.core_sections.contains_key(channel_id)
            || self.canvas_sections.contains_key(channel_id)
    }
}

/// An agent with its session state, owned by the pool or a running task.
pub struct OwnedAgent {
    pub index: usize,
    pub acp: AcpClient,
    pub state: SessionState,
    /// Model catalog from first session/new. None until first session created.
    pub model_capabilities: Option<AgentModelCapabilities>,
    /// Desired model ID (from `Config.model`). Applied after every `session_new_full()`.
    pub desired_model: Option<String>,
    /// Whether `desired_model` was set by a live `SwitchModel` control signal
    /// (as opposed to being derived from config/persona at spawn). Used by the
    /// desktop reader to distinguish a genuine runtime override from a stale
    /// session whose persona model was edited. Reset on spawn/restart.
    pub model_overridden: bool,
    /// Normalized agent name from initialize (`agentInfo.name`/`serverInfo.name`).
    pub agent_name: String,
    /// Whether Goose accepted its custom system-prompt method. `None` probes on
    /// the first session; method-not-found is cached as `Some(false)` so legacy
    /// user-message framing is used for this process thereafter.
    pub goose_system_prompt_supported: Option<bool>,
    /// Protocol version reported by the agent in its initialize response.
    pub protocol_version: u32,
}

/// Package name reported by `claude-agent-acp` in its `initialize` response.
/// Any adapter reporting this name supports `_meta.systemPrompt: {append: ...}`
/// on `session/new` â€” the feature landed in v0.6.0 (Oct 2025), before the
/// `@zed-industries/claude-code-acp` â†’ `@agentclientprotocol/claude-agent-acp`
/// rename, so the new name is a reliable capability gate.
const CLAUDE_AGENT_ACP_NAME: &str = "@agentclientprotocol/claude-agent-acp";

/// Prompt instruction enabled only after the adapter acknowledges the v1
/// host-final capability. The host, not a tool command, owns the fixed-route
/// ordinary answer; deliberate additional or destination-changing sends remain
/// on the existing permissioned CLI path.
const HOST_FINAL_REPLY_PROMPT: &str = "[Host Final Reply]\n\
Your normal final assistant response for this channel turn is delivered by the host. \
Do not run `buzz messages send` for that same answer. Use explicit messaging only \
for an intentional additional, proactive, or different-destination message; those \
actions remain permissioned.";

fn has_system_prompt_support(
    protocol_version: u32,
    agent_name: &str,
    goose_system_prompt_supported: Option<bool>,
) -> bool {
    if agent_name == "goose" {
        goose_system_prompt_supported == Some(true)
    } else if agent_name == CLAUDE_AGENT_ACP_NAME {
        true
    } else {
        protocol_version >= 2
    }
}

fn session_new_system_prompt<'a>(
    is_goose: bool,
    protocol_version: u32,
    agent_name: &str,
    prompt: Option<&'a str>,
) -> Option<SystemPromptTransport<'a>> {
    if is_goose || (protocol_version < 2 && agent_name != CLAUDE_AGENT_ACP_NAME) {
        None
    } else if agent_name == CLAUDE_AGENT_ACP_NAME {
        prompt.map(SystemPromptTransport::ClaudeMeta)
    } else {
        prompt.map(SystemPromptTransport::Field)
    }
}

impl OwnedAgent {
    pub(crate) fn has_system_prompt_support(&self) -> bool {
        has_system_prompt_support(
            self.protocol_version,
            &self.agent_name,
            self.goose_system_prompt_supported,
        )
    }
}

/// Pool of agents with take-and-return ownership semantics.
///
/// Agents are either idle (sitting in `agents[i]`) or checked out
/// (running inside a spawned task). The `task_map` tracks in-flight
/// tasks for panic recovery.
pub struct AgentPool {
    agents: Vec<Option<OwnedAgent>>,
    result_tx: mpsc::UnboundedSender<PromptResult>,
    result_rx: mpsc::UnboundedReceiver<PromptResult>,
    pub join_set: JoinSet<()>,
    task_map: HashMap<tokio::task::Id, TaskMeta>,
}

/// Result returned by a completed prompt task.
pub struct PromptResult {
    pub agent: OwnedAgent,
    pub source: PromptSource,
    /// Identifies the completed turn for observer terminal events.
    pub turn_id: String,
    pub outcome: PromptOutcome,
    /// Present on failure in Queue mode, for requeue.
    pub batch: Option<FlushBatch>,
}

/// Whether the prompt came from a channel event or a heartbeat.
#[derive(Debug)]
pub enum PromptSource {
    Channel(Uuid),
    Heartbeat,
}

/// Apply state effects for Race 1, where a control signal arrives just after the
/// prompt completed naturally. The prompt result has already been consumed by
/// `select!`, so the harness must synthesize a successful result while still
/// honoring any load-bearing control signal semantics.
fn apply_completed_before_control_signal(
    state: &mut SessionState,
    source: &PromptSource,
    control_signal: &ControlSignal,
) {
    // Rotate and SwitchModel both invalidate so the next turn creates a fresh
    // session. For SwitchModel the caller has already set `desired_model`, so
    // the fresh session applies the new model on its next creation.
    if matches!(
        control_signal,
        ControlSignal::Rotate | ControlSignal::SwitchModel(_)
    ) {
        state.invalidate(source);
    }
}

/// Control signal for an in-flight channel turn.
///
/// Not `Copy`: `SwitchModel` carries an owned `String`. Callers must clone when
/// a value is needed after a move, or match by reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlSignal {
    /// Stop the current turn and drop its triggering batch.
    Cancel,
    /// Stop the current turn and requeue its triggering batch for a merged
    /// re-prompt framed as a **supersede**: the new request replaces the old.
    Interrupt,
    /// Stop the current turn and requeue its triggering batch for a merged
    /// re-prompt framed as a **steer**: a message arrived while the agent was
    /// working; it should continue its work and incorporate the message if
    /// relevant, not treat it as a replacement task. This is the default
    /// mid-turn delivery path (see [`MultipleEventHandling::Steer`]).
    Steer,
    /// Stop the current turn and drop its triggering batch. The session is
    /// invalidated just like cancel; the next turn creates a fresh session.
    Rotate,
    /// Switch the agent's model, then requeue the triggering batch so it
    /// re-runs on a fresh session under the new model. The model lands by
    /// setting `OwnedAgent::desired_model` before invalidation; the requeued
    /// turn re-creates the session and re-applies `desired_model`. Runtime-only
    /// â€” never persisted, gone on restart/respawn.
    SwitchModel(String),
}

/// Goose-native non-cancelling steer request, sent from the main loop to an
/// in-flight prompt task's read loop via a capacity-1 mpsc channel.
///
/// The read loop owns the `AcpClient`'s reader/writer for the duration of the
/// turn, so we cannot drive a steer write from the main thread directly. The
/// main loop carries the steer prompt body (already framed by
/// `queue::native_steer_framing()` + `queue::format_event_block`); the read
/// loop completes `sessionId` (lexical) and `expectedRunId`
/// (`AcpClient::active_run_id` at write time) when it actually emits the
/// JSON-RPC request. The main loop awaits a `SteerAck` on the `ack_tx`
/// oneshot.
///
/// ## Why the read loop fills params, not the main loop
///
/// `expectedRunId` is a *moving target*: the read loop updates
/// `self.active_run_id` as goose emits `session/update` notifications, and
/// the steer is rejected if the supplied id doesn't match the *current* run.
/// A snapshot taken at dispatch (or at mode-gate time) can be stale by the
/// time the read loop actually writes the steer line. Filling params at
/// write tó¿;öÚ$z{-®éÜj×FFCrÓ–RÖfcvS–SCƒfR#° ¢òòò'V–ÆB&VÂÂ7'—Föw&†–6ÆÇ’6–væVBæ÷7G"6çf2WfVçBf÷"FW7G2à¢òòğ¢òòò–æ6ÇVFW2F†R6÷'&V7B¶–æBƒC’æBâ†Fr6''––ær4„ääTÅõUT”F ¢òòò6òÆÂ7G'V7GW&ÂæB6öçFVçBfÆ–FF–öç272à¢fâÖ¶Uö6çf5öWfVçE÷fÇVR†6öçFVçC¢g7G"’Óâ6W&FUö§6öã£¥fÇVR°¢ÆWB¶W—2Ò¶W—3£¦vVæW&FR‚“°¢ÆWB…÷FrÒFs£§'6R…²&‚"Â4„ääTÅõUT”EÒ’æW‡V7B‚&‚Fr"“°¢ÆWBWfVçBÒWfVçD'V–ÆFW#£¦æWr„¶–æC£¤7W7FöÒ†'W§¥ö6÷&S£¦¶–æC£¤´”äEô4åd22Sb’Â6öçFVçB¢çFw2…¶…÷FuÒ¢ç6–vå÷v—F…ö¶W—2‚f¶W—2¢æW‡V7B‚'6–vâ"“°¢6W&FUö§6öã£§Fõ÷fÇVR‚fWfVçB’æW‡V7B‚'6W&–Æ—6R"¢Ğ ¢5·FW7EĞ¢fâFW7Eö6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6Uö†•÷F‚‚’°¢ÆWBWbÒÖ¶Uö6çf5öWfVçE÷fÇVR‚"2FVÒ–ç7G'V7F–öç5Æä&R†VÇgVÂâ"“°¢ÆWB–BÒWe²&–B%Òæ5÷7G"‚’çVçw&‚’çFõ÷7G&–ær‚“°¢ÆWB&W7VÇBÒ6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6R‚e¶WeÒÂ4„ääTÅõUT”B“°¢ÆWB6V7F–öâÒ&W7VÇBæW‡V7B‚&W‡V7FVB6öÖR"“°¢76W'B‡6V7F–öâæ6öçF–ç2‚f–B’Â'6V7F–öâ×W7B6öçF–âF†RWfVçB–B"“°¢76W'B‡6V7F–öâæ6öçF–ç2‚&'W§¢6çf2vWBÒÖ6†ææVÂ"’“°¢76W'B‡6V7F–öâæ6öçF–ç2„4„ääTÅõUT”B’“°¢76W'B‡6V7F–öâç7F'G5÷v—F‚‚%´6†ææVÂ6çf5Ò"’“°¢òòF–ÖW7F××W7BW6R¢7Vff—‚Âæ÷B³£ ¢76W'B‡6V7F–öâæ6öçF–ç2‚u¢r’Â'F–ÖW7F××W7BW6R¢7Vff—‚"“°¢Ğ ¢5·FW7EĞ¢fâFW7Eö6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6UöV×G•ö'&•÷&WGW&ç5öæöæR‚’°¢ÆWB&W7VÇBÒ6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6R‚eµÒÂ4„ääTÅõUT”B“°¢76W'B‡&W7VÇBæ—5öæöæR‚’“°¢Ğ ¢5·FW7EĞ¢fâFW7Eö6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6Uö&Ææµö6öçFVçE÷&WGW&ç5öæöæR‚’°¢ÆWBWbÒÖ¶Uö6çf5öWfVçE÷fÇVR‚""“°¢ÆWB&W7VÇBÒ6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6R‚e¶WeÒÂ4„ääTÅõUT”B“°¢76W'B€¢&W7VÇBæ—5öæöæR‚’À¢&&Ææ²6öçFVçB×W7B&WGW&âæöæR†6ÆV&VB6çf2’ ¢“°¢Ğ ¢5·FW7EĞ¢fâFW7Eö6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6UöV×G•ö6öçFVçE÷&WGW&ç5öæöæR‚’°¢ÆWBWbÒÖ¶Uö6çf5öWfVçE÷fÇVR‚""“°¢ÆWB&W7VÇBÒ6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6R‚e¶WeÒÂ4„ääTÅõUT”B“°¢76W'B‡&W7VÇBæ—5öæöæR‚’“°¢Ğ ¢òòò&&R¥4ôâö&¦V7Bv—F‚ÆW6–&ÆRÖÆöö¶–ær–B'WBÖ—76–ærV&¶W’÷6–rö¶–æB÷Fw0¢òòò×W7B&R&V¦V7FVB(	Bæ÷B6–ÆVçFÇ’66WFVBv—F‚'F–ÂÖWFFFà¢5·FW7EĞ¢fâFW7Eö6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6U÷'F–Åöö&¦V7E÷&WGW&ç5öæöæR‚’°¢ÆWB'F–ÂÒ6W&FUö§6öã£¦§6öâ‡°¢&–B#¢&#&36CFSVcf#&36CFSVcf#&36CFSVcf#&36CFSVcf#&36CFSVcf#""À¢&7&VFVEöB#¢sS3##ö“cBÀ¢&6öçFVçB#¢'6öÖR–ç7G'V7F–öç2 ¢Ò“°¢ÆWB&W7VÇBÒ6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6R‚e·'F–ÅÒÂ4„ääTÅõUT”B“°¢76W'B€¢&W7VÇBæ—5öæöæR‚’À¢''F–ÂWfVçBö&¦V7B†Ö—76–ærV&¶W’÷6–rö¶–æB÷Fw2’×W7B&WGW&âæöæR ¢“°¢Ğ ¢òòò¥4ôâö&¦V7BF†BÆöö·2Æ–¶RâWfVçB'WB†27&VFVEöF27G&–æp¢òòò×W7B&R&V¦V7FVB(	BF†Ræ÷7G#£¤WfVçB'6W"Væf÷&6W2–çFVvW"G—Rà¢5·FW7EĞ¢fâFW7Eö6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6U÷7G&–æu÷F–ÖW7F×÷&WGW&ç5öæöæR‚’°¢ÆWB¶W—2Ò¶W—3£¦vVæW&FR‚“°¢ÆWB…÷FrÒFs£§'6R…²&‚"Â4„ääTÅõUT”EÒ’æW‡V7B‚&‚Fr"“°¢ÆWB×WBWbÒ6W&FUö§6öã£§Fõ÷fÇVR€¢WfVçD'V–ÆFW#£¦æWr„¶–æC£¤7W7FöÒ†'W§¥ö6÷&S£¦¶–æC£¤´”äEô4åd22Sb’Â&6öçFVçB"¢çFw2…¶…÷FuÒ¢ç6–vå÷v—F…ö¶W—2‚f¶W—2¢æW‡V7B‚'6–vâ"’À¢¢æW‡V7B‚'6W&–Æ—6R"“°¢òò6÷''WB7&VFVEöBFò7G&–ærfÇVRà¢We²&7&VFVEöB%ÒÒ6W&FUö§6öã£¥fÇVS£¥7G&–ær‚###bÓ2ÓUCc£3£³£"æ–çFò‚’“°¢ÆWB&W7VÇBÒ6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6R‚e¶WeÒÂ4„ääTÅõUT”B“°¢76W'B€¢&W7VÇBæ—5öæöæR‚’À¢'7G&–ær7&VFVEöB×W7B&R&V¦V7FVB'’æ÷7G#£¤WfVçBFW6W&–Æ—6W" ¢“°¢Ğ ¢òòò¥4ôâö&¦V7BF†BÆöö·2Æ–¶RâWfVçB'WB—2Ö—76–ær7&VFVEöF ¢òòò×W7B&R&V¦V7FVB(	Bæ÷7G#£¤WfVçB&WV—&W2F†Rf–VÆBà¢5·FW7EĞ¢fâFW7Eö6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6UöÖ—76–æu÷F–ÖW7F×÷&WGW&ç5öæöæR‚’°¢ÆWB¶W—2Ò¶W—3£¦vVæW&FR‚“°¢ÆWB…÷FrÒFs£§'6R…²&‚"Â4„ääTÅõUT”EÒ’æW‡V7B‚&‚Fr"“°¢ÆWB×WBWbÒ6W&FUö§6öã£§Fõ÷fÇVR€¢WfVçD'V–ÆFW#£¦æWr„¶–æC£¤7W7FöÒ†'W§¥ö6÷&S£¦¶–æC£¤´”äEô4åd22Sb’Â&6öçFVçB"¢çFw2…¶…÷FuÒ¢ç6–vå÷v—F…ö¶W—2‚f¶W—2¢æW‡V7B‚'6–vâ"’À¢¢æW‡V7B‚'6W&–Æ—6R"“°¢Wbæ5öö&¦V7Eö×WB‚’çVçw&‚’ç&VÖ÷fR‚&7&VFVEöB"“°¢ÆWB&W7VÇBÒ6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6R‚e¶WeÒÂ4„ääTÅõUT”B“°¢76W'B€¢&W7VÇBæ—5öæöæR‚’À¢&Ö—76–ær7&VFVEöB×W7B&R&V¦V7FVB'’æ÷7G#£¤WfVçBFW6W&–Æ—6W" ¢“°¢Ğ ¢òòòâWfVçBv—F‚F–ÖW7F×BF–ÖW7F×£¦Ö‚‚’‡ScC£¤Ô‚’×W7B&WGW&âæöæRà¢òòğ¢òòòScC£¤Ô‚2“cFw&2FòÓÂv†–6‚6‡&öæò6–ÆVçFÇ’66WG20¢òòò“c’Ó"Ó3C#3£S“£S•¢âF†R6†V6¶VB“cC£§G'•ög&öÒ×W7B&V¦V7B—Bf—'7Bà¢5·FW7EĞ¢fâFW7Eö6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6U÷F–ÖW7F×öÖ…÷&WGW&ç5öæöæR‚’°¢ÆWB¶W—2Ò¶W—3£¦vVæW&FR‚“°¢ÆWB…÷FrÒFs£§'6R…²&‚"Â4„ääTÅõUT”EÒ’æW‡V7B‚&‚Fr"“°¢ÆWBWbÒ6W&FUö§6öã£§Fõ÷fÇVR€¢WfVçD'V–ÆFW#£¦æWr„¶–æC£¤7W7FöÒ†'W§¥ö6÷&S£¦¶–æC£¤´”äEô4åd22Sb’Â&6öçFVçB"¢çFw2…¶…÷FuÒ¢æ7W7FöÕö7&VFVEöB…F–ÖW7F×£¦Ö‚‚’¢ç6–vå÷v—F…ö¶W—2‚f¶W—2¢æW‡V7B‚'6–vâ"’À¢¢æW‡V7B‚'6W&–Æ—6R"“°¢ÆWB&W7VÇBÒ6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6R‚e¶WeÒÂ4„ääTÅõUT”B“°¢76W'B€¢&W7VÇBæ—5öæöæR‚’À¢%F–ÖW7F×£¦Ö‚‚’‡ScC£¤Ô‚’×W7B&WGW&âæöæR(	Bæ÷Bw&Fò“c’ ¢“°¢Ğ ¢òòò7G'V7GW&ÆÇ’6ö×ÆWFR'WBF×W&VBWfVçB†6öçFVçBÇFW&VBgFW"6–væ–ær¢òòò×W7B&R&V¦V7FVB'’WfVçBçfW&–g’‚’à¢5·FW7EĞ¢fâFW7Eö6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6U÷F×W&VEöWfVçE÷&WGW&ç5öæöæR‚’°¢ÆWB¶W—2Ò¶W—3£¦vVæW&FR‚“°¢ÆWB…÷FrÒFs£§'6R…²&‚"Â4„ääTÅõUT”EÒ’æW‡V7B‚&‚Fr"“°¢ÆWB×WBWbÒ6W&FUö§6öã£§Fõ÷fÇVR€¢WfVçD'V–ÆFW#£¦æWr€¢¶–æC£¤7W7FöÒ†'W§¥ö6÷&S£¦¶–æC£¤´”äEô4åd22Sb’À¢&÷&–v–æÂ"À¢¢çFw2…¶…÷FuÒ¢ç6–vå÷v—F…ö¶W—2‚f¶W—2¢æW‡V7B‚'6–vâ"’À¢¢æW‡V7B‚'6W&–Æ—6R"“°¢òòF×W"F†R6öçFVçBgFW"6–væ–ær(	B–BæB6–ræòÆöævW"w&VRà¢We²&6öçFVçB%ÒÒ6W&FUö§6öã£¥fÇVS£¥7G&–ær‚&–æ¦V7FVB–ç7G'V7F–öç2"æ–çFò‚’“°¢ÆWB&W7VÇBÒ6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6R‚e¶WeÒÂ4„ääTÅõUT”B“°¢76W'B€¢&W7VÇBæ—5öæöæR‚’À¢'F×W&VBWfVçB×W7Bf–ÂfW&–g’‚’æB&WGW&âæöæR ¢“°¢Ğ ¢òòòâWfVçBv—F‚F†Rw&öær¶–æB†æ÷BC’×W7B&R&V¦V7FVBà¢5·FW7EĞ¢fâFW7Eö6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6U÷w&öæuö¶–æE÷&WGW&ç5öæöæR‚’°¢ÆWB¶W—2Ò¶W—3£¦vVæW&FR‚“°¢ÆWB…÷FrÒFs£§'6R…²&‚"Â4„ääTÅõUT”EÒ’æW‡V7B‚&‚Fr"“°¢ÆWBWbÒ6W&FUö§6öã£§Fõ÷fÇVR€¢WfVçD'V–ÆFW#£¦æWr„¶–æC£¤7W7FöÒƒ’’Â&6öçFVçB"¢çFw2…¶…÷FuÒ¢ç6–vå÷v—F…ö¶W—2‚f¶W—2¢æW‡V7B‚'6–vâ"’À¢¢æW‡V7B‚'6W&–Æ—6R"“°¢ÆWB&W7VÇBÒ6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6R‚e¶WeÒÂ4„ääTÅõUT”B“°¢76W'B‡&W7VÇBæ—5öæöæR‚’Â'w&öær¶–æB×W7B&WGW&âæöæR"“°¢Ğ ¢òòòâWfVçBÖ—76–ærF†RW‡V7FVB‚×Fr†÷"6''––ærF–ffW&VçB6†ææVÂUT”B¢òòò×W7B&R&V¦V7FVBà¢5·FW7EĞ¢fâFW7Eö6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6U÷w&öæuö…÷Fu÷&WGW&ç5öæöæR‚’°¢ÆWB¶W—2Ò¶W—3£¦vVæW&FR‚“°¢ÆWBw&öæuö‚ÒFs£§'6R…²&‚"Â&Ö&&&"Ö6662ÖFFFBÖVVVVVVVVVVVR%Ò’æW‡V7B‚&‚Fr"“°¢ÆWBWbÒ6W&FUö§6öã£§Fõ÷fÇVR€¢WfVçD'V–ÆFW#£¦æWr„¶–æC£¤7W7FöÒ†'W§¥ö6÷&S£¦¶–æC£¤´”äEô4åd22Sb’Â&6öçFVçB"¢çFw2…·w&öæuö…Ò¢ç6–vå÷v—F…ö¶W—2‚f¶W—2¢æW‡V7B‚'6–vâ"’À¢¢æW‡V7B‚'6W&–Æ—6R"“°¢ÆWB&W7VÇBÒ6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6R‚e¶WeÒÂ4„ääTÅõUT”B“°¢76W'B‡&W7VÇBæ—5öæöæR‚’Â&Ö—6ÖF6†VB‚×Fr×W7B&WGW&âæöæR"“°¢Ğ ¢5·FW7EĞ¢fâFW7Eö6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6U÷F–ÖW7F×÷W6W5÷¥÷7Vff—‚‚’°¢ÆWBWbÒÖ¶Uö6çf5öWfVçE÷fÇVR‚&–ç7G'V7F–öç2"“°¢ÆWB&W7VÇBÒ6çf5÷6V7F–öåög&öÕ÷VW'•÷&W7öç6R‚e¶WeÒÂ4„ääTÅõUT”B“°¢ÆWB6V7F–öâÒ&W7VÇBæW‡V7B‚'fÆ–BWfVçB×W7B&öGV6R6V7F–öâ"“°¢76W'B€¢6V7F–öâæ6öçF–ç2‚u¢r’À¢%$d3333’F–ÖW7F××W7BW6R¢7Vff—‚Âæ÷B³£ ¢“°¢76W'B€¢6V7F–öâæ6öçF–ç2‚"³£"’À¢'F–ÖW7F××W7Bæ÷BW6R³£öfg6WB ¢“°¢Ğ ¢òò)H)HæWr×6W76–öâ6†ææVÂ6öçFW‡B†öæR&W6öÇfRÂGvò6öç7VÖW'2’)H)H)H)H)H)H)H)H)H)H)H)H)H  ¢òòò¶6†ææVÄ–æfõ&W6öÇfW&Òv†÷6RÆ§’$U5BfÆÆ&6²—26W'fVB'’Æö6À¢òòò…EE6W'fW"ÂÇW26÷VçFW"öbF†R&WVW7G2F†B7GVÆÇ’&V6†VB—Bà¢òòò6÷VçF–ær&VÂ&WVW7G2—2F†Rö–çC¢F†R6ö×÷6—F–öâFW7G2&RW&Ræ@¢òòò6ææ÷B6VRGWÆ–6FVB’ôòà¢7–æ2fâ6÷VçF–æu÷&W6öÇfW"€¢&W7öç6S¢6W&FUö§6öã£¥fÇVRÀ¢’Óâ€¢6†ææVÄ–æfõ&W6öÇfW"À¢7FC£§7–æ3£¤&3Ç7FC£§7–æ3£¦FöÖ–3£¤FöÖ–5W6—¦SâÀ¢Fö¶–ó£§F6³£¤¦ö–ä†æFÆSÂ‚“âÀ¢’°¢W6R7FC£§7–æ3£¦FöÖ–3£§´FöÖ–5W6—¦RÂ÷&FW&–æwÓ°¢W6RFö¶–ó£¦–ó£§´7–æ5&VDW‡BÂ7–æ5w&—FTW‡GÓ° ¢ÆWBÆ—7FVæW"ÒFö¶–ó£¦æWC£¥F7Æ—7FVæW#£¦&–æB‚##rããã£"¢æv—@¢æW‡V7B‚&&–æBFW7B…EE6W'fW""“°¢ÆWB&6U÷W&ÂÒf÷&ÖB‚&‡GG¢ò÷·Ò"ÂÆ—7FVæW"æÆö6ÅöFG"‚’çVçw&‚’“°¢ÆWB&WVW7G2Ò7FC£§7–æ3£¤&3£¦æWr„FöÖ–5W6—¦S£¦æWrƒ’“°¢ÆWB6W'fW%÷&WVW7G2Ò&WVW7G2æ6ÆöæR‚“°¢ÆWB&öG’Ò&W7öç6RçFõ÷7G&–ær‚“°¢ÆWB6W'fW"ÒFö¶–ó£§7vâ†7–æ2Ö÷fR°¢v†–ÆRÆWBö²‚†×WB6ö6¶WBÂò’’ÒÆ—7FVæW"æ66WB‚’æv—B°¢ÆWB×WB'VbÒfV2³²ƒ“%Ó°¢ÆWBòÒ6ö6¶WBç&VB‚f×WB'Vb’æv—C°¢6W'fW%÷&WVW7G2æfWF6…öFBƒÂ÷&FW&–æs£¥6W77B“°¢ÆWB&W7öç6RÒf÷&ÖB€¢$…EEóã#ôµÇ%Æä6öçFVçBÕG—S¢Æ–6F–öâö§6öåÇ%Æä6öçFVçBÔÆVæwFƒ¢·ÕÇ%Æä6öææV7F–öã¢6Æ÷6UÇ%ÆåÇ%Æç·Ò"À¢&öG’æÆVâ‚’À¢&öG¢“°¢ÆWBòÒ6ö6¶WBçw&—FUöÆÂ‡&W7öç6Ræ5ö'—FW2‚’’æv—C°¢Ğ¢Ò“°¢ÆWB&W7BÒ7&FS£§&VÆ“£¥&W7D6Æ–VçB°¢‡GG¢&WvW7C£¤6Æ–VçC£¦æWr‚’À¢&6U÷W&ÂÀ¢¶W—3¢æ÷7G#£¤¶W—3£¦vVæW&FR‚’À¢WF…÷Fs¢æöæRÀ¢WF…÷Fuö§6öã¢æöæRÀ¢Ó°¢€¢6†ææVÄ–æfõ&W6öÇfW#£¦æWr‡7FC£¦6öÆÆV7F–öç3£¤†6„Ö£¦æWr‚’Â&W7B’À¢&WVW7G2À¢6W'fW"À¢¢Ğ ¢fâ6†ææVÅöÖWFFF÷&W7öç6R†–C¢WV–BÂFw3¢eµ²g7G#²%ÕÒ’Óâ6W&FUö§6öã£¥fÇVR°¢ÆWB×WBWfVçE÷Fw2ÒfV2¶§6öâ…²&B"Â–BçFõ÷7G&–ær‚•Ò•Ó°¢WfVçE÷Fw2æW‡FVæB‡Fw2æ—FW"‚’æÖ‡Å¶²Âe×Â§6öâ…¶²ÂeÒ’’“°¢§6öâ…·²'Fw2#¢WfVçE÷Fw2ÕÒ¢Ğ ¢òòòæ÷&ÖÂ6†ææVÂ––VÆG2æöâÔDÒ†6çf2ÆÆ÷vVB’æB—G2æÖRf÷"F†P¢òòòF—FÆR7Vff—‚(	BæBF†R6V6öæB6öç7VÖW"&VG2—Bg&öÒ66†RÂæ÷BF†Rv—&Rà¢5·Fö¶–ó£§FW7EĞ¢7–æ2fâFW7EöæWu÷6W76–öåö6†ææVÅö6öçFW‡E÷VÆ–f–W5ööæ÷&ÖÅö6†ææVÂ‚’°¢W6R7FC£§7–æ3£¦FöÖ–3£¤÷&FW&–æs° ¢ÆWB–BÒWV–C£¦æWu÷cB‚“°¢ÆWB&W7öç6RÒ6†ææVÅöÖWFFF÷&W7öç6R†–BÂeµ²&æÖR"Â&'W§¢ÖFWb%ÒÂ²'B"Â'7G&VÒ%ÕÒ“°¢ÆWB‡&W6öÇfW"Â&WVW7G2Â6W'fW"’Ò6÷VçF–æu÷&W6öÇfW"‡&W7öç6R’æv—C° ¢ÆWB†—5öFÒÂF—FÆUö6†ææVÂÂ6†ææVÅ÷G—R’Ğ¢&W6öÇfUöæWu÷6W76–öåö6†ææVÅö6öçFW‡B‚g&W6öÇfW"Â–B’æv—C°¢76W'B‚—5öFÒÂ&7G&VÒ6†ææVÂ—2æ÷BDÒ"“°¢76W'EöW‡F—FÆUö6†ææVÂæ5öFW&Vb‚’Â6öÖR‚&'W§¢ÖFWb"’“°¢76W'EöW†6†ææVÅ÷G—Ræ5öFW&Vb‚’Â6öÖR‚'7G&VÒ"’“°¢76W'EöW‡&WVW7G2æÆöB„÷&FW&–æs£¥6W77B’Â“° ¢ÆWB…òÂv–âÂò’Ò&W6öÇfUöæWu÷6W76–öåö6†ææVÅö6öçFW‡B‚g&W6öÇfW"Â–B’æv—C°¢76W'EöW†v–âæ5öFW&Vb‚’Â6öÖR‚&'W§¢ÖFWb"’“°¢76W'EöW€¢&WVW7G2æÆöB„÷&FW&–æs£¥6W77B’À¢À¢&&W6öÇfVB6†ææVÂ—266†VB(	Bæò6V6öæBÆöö·W ¢“°¢6W'fW"æ&÷'B‚“°¢Ğ ¢òòòDÒ6'&–W2æòW6VgVÂæÖRÂ6ò—BvWG2F†R&&RvVçBF—FÆR†æBæğ¢òòò6çf26V7F–öâ’à¢5·Fö¶–ó£§FW7EĞ¢7–æ2fâFW7EöæWu÷6W76–öåö6†ææVÅö6öçFW‡EöÆVfW5ööFÕ÷VçVÆ–f–VB‚’°¢ÆWB–BÒWV–C£¦æWu÷cB‚“°¢ÆWB&W7öç6RÒ6†ææVÅöÖWFFF÷&W7öç6R†–BÂeµ²&æÖR"Â$DÒ%ÒÂ²'B"Â&FÒ%ÕÒ“°¢ÆWB‡&W6öÇfW"Â÷&WVW7G2Â6W'fW"’Ò6÷VçF–æu÷&W6öÇfW"‡&W7öç6R’æv—C° ¢ÆWB†—5öFÒÂF—FÆUö6†ææVÂÂ6†ææVÅ÷G—R’Ğ¢&W6öÇfUöæWu÷6W76–öåö6†ææVÅö6öçFW‡B‚g&W6öÇfW"Â–B’æv—C°¢76W'B†—5öFÒ“°¢76W'EöW†6†ææVÅ÷G—Ræ5öFW&Vb‚’Â6öÖR‚&FÒ"’“°¢76W'EöW€¢F—FÆUö6†ææVÂÂæöæRÀ¢&DÒæÖR×W7BæWfW"&V6‚F†R6W76–öâF—FÆR ¢“°¢6W'fW"æ&÷'B‚“°¢Ğ ¢òòòF†R'Væ¶æ÷vâ&Æ6V†öÆFW"fWF6…ö6†ææVÅö–æfö7V'7F—GWFW2f÷"¢òòòÖWFFFWfVçBv—F‚æòæÖVFr—2æ÷B6†ææVÂæÖS¢VÆ–g––ærv—F€¢òòò—Bv÷VÆBF—FÆRWfW'’VææÖVB6†ææVÂvVçB+r7Væ¶æ÷væà¢5·Fö¶–ó£§FW7EĞ¢7–æ2fâFW7EöæWu÷6W76–öåö6†ææVÅö6öçFW‡E÷G&VG5÷F†U÷Væ¶æ÷våöæÖUö5ö'6VçB‚’°¢ÆWB–BÒWV–C£¦æWu÷cB‚“°¢ÆWB&W7öç6RÒ6†ææVÅöÖWFFF÷&W7öç6R†–BÂeµ²'B"Â'7G&VÒ%ÕÒ“°¢ÆWB‡&W6öÇfW"Â÷&WVW7G2Â6W'fW"’Ò6÷VçF–æu÷&W6öÇfW"‡&W7öç6R’æv—C° ¢ÆWB†—5öFÒÂF—FÆUö6†ææVÂÂò’Ò&W6öÇfUöæWu÷6W76–öåö6†ææVÅö6öçFW‡B‚g&W6öÇfW"Â–B’æv—C°¢76W'B‚—5öFÒÂ&æÖVÆW727G&VÒ6†ææVÂ—27F–ÆÂæ÷BDÒ"“°¢76W'EöW€¢F—FÆUö6†ææVÂÂæöæRÀ¢'F†RVæ¶æ÷væÆ6V†öÆFW"×W7B––VÆB&&RF—FÆR ¢“°¢6W'fW"æ&÷'B‚“°¢Ğ ¢òòòâVç&W6öÇf&ÆR6†ææVÂ––VÆG2F†R&&RF—FÆRÂf–Ç26Æ÷6VB2DÒÂæ@¢òòò6÷7G2W†7FÇ’ôäRfWF6…ö6†ææVÅö–æfö6WVVæ6R(	BGvòGFV×G2Â&V6W6P¢òòòfWF6…÷v—F…÷&WG'–&WG&–W2öæ6Râ&W6öÇfR‚–66†W2öæÇ’6öÖVÂ6ò¢òòò6V6öæB&W6öÇfRf÷"F†RF—FÆRv÷VÆBF÷V&ÆRF†—2–âg&öçBöb6W76–öâöæWvÀ¢òòòW†7FÇ’v†VâF†R&VÆ’—2Ç&VG’FVw&FVBà¢5·Fö¶–ó£§FW7EĞ¢7–æ2fâFW7EöæWu÷6W76–öåö6†ææVÅö6öçFW‡EöGFV×G5öå÷Vç&W6öÇfVEö6†ææVÅööæ6R‚’°¢W6R7FC£§7–æ3£¦FöÖ–3£¤÷&FW&–æs° ¢ÆWB‡&W6öÇfW"Â&WVW7G2Â6W'fW"’Ò6÷VçF–æu÷&W6öÇfW"†§6öâ…µÒ’’æv—C° ¢ÆWB†—5öFÒÂF—FÆUö6†ææVÂÂ6†ææVÅ÷G—R’Ğ¢&W6öÇfUöæWu÷6W76–öåö6†ææVÅö6öçFW‡B‚g&W6öÇfW"ÂWV–C£¦æWu÷cB‚’’æv—C°¢76W'B†—5öFÒÂ&âVæFWFW&Ö–æ&ÆR6†ææVÂG—R×W7Bf–Â6Æ÷6VB"“°¢76W'EöW‡F—FÆUö6†ææVÂÂæöæRÂ'Vç&W6öÇfVB6†ææVÇ2vWB&&RF—FÆR"“°¢76W'EöW†6†ææVÅ÷G—RÂæöæR“°¢76W'EöW€¢&WVW7G2æÆöB„÷&FW&–æs£¥6W77B’À¢"À¢&öæRfWF6…ö6†ææVÅö–æfò6WVVæ6R†–æ—F–ÂGFV×B²6–ævÆR&WG'’’ ¢“°¢6W'fW"æ&÷'B‚“°¢Ğ§Ğ