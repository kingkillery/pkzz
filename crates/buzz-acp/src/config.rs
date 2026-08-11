//! Configuration for the buzz-acp harness.
//!
//! CLI-first: every option is a CLI flag with env var fallback.
//! Config file (TOML) for complex subscription rules.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use clap::Parser;
use clap::ValueEnum;
use nostr::Keys;
use thiserror::Error;
use url::Url;
use uuid::Uuid;

use crate::filter::SubscriptionRule;
use crate::ompk_execution::OmpkExecutionPolicy;

/// Default idle timeout (seconds) when neither `--idle-timeout` nor the
/// deprecated `--turn-timeout` is set.
///
/// Sized for slow turns where the agent may go silent on its outer ACP channel
/// while running long sub-tools (e.g. a buzz-agent running another agent, or
/// codex/claude doing multi-minute single tool calls). 900s gives 300s of
/// breathing room above the 600s max shell timeout, so legitimate long-running
/// tool calls don't race the idle deadline.
/// Override via `--idle-timeout` / `BUZZ_ACP_IDLE_TIMEOUT`.
pub(crate) const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 900;

/// Default absolute wall-clock cap per agent turn (2 hours).
/// Override via `--max-turn-duration` / `BUZZ_ACP_MAX_TURN_DURATION`.
pub(crate) const DEFAULT_MAX_TURN_DURATION_SECS: u64 = 7200;

/// Upper bound for `max_turn_duration` (7 days). Any higher is operationally
/// meaningless and risks arithmetic overflow when deriving the in-flight
/// deadline (`max_turn_duration + IN_FLIGHT_DEADLINE_BUFFER_SECS`).
pub(crate) const MAX_TURN_DURATION_CEILING_SECS: u64 = 604_800;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to parse nostr keys: {0}")]
    KeyParse(#[from] nostr::key::Error),

    #[error("failed to read file: {0}")]
    Io(#[from] std::io::Error),

    #[error("config file error: {0}")]
    ConfigFile(String),
}

#[derive(Debug, Clone, PartialEq, clap::ValueEnum)]
pub enum SubscribeMode {
    Mentions,
    All,
    Config,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum DedupMode {
    Drop,
    Queue,
}

/// How to handle new @mentions while a turn is already in-flight for that channel.
#[derive(Debug, Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum MultipleEventHandling {
    /// Queue new events while a turn is in-flight. Deliver after current turn
    /// completes. Existing behavior â€” zero code change in this path.
    Queue,
    /// Cancel the in-flight turn and re-dispatch a merged prompt that frames
    /// the new events as a **steering message** â€” one that arrived while the
    /// agent was working, to be woven into the in-progress task rather than
    /// treated as a replacement. Fires for any author the inbound author gate
    /// admits (owner âˆª allowlist âˆª siblings). This is the default mid-turn
    /// delivery path. Requires DedupMode::Queue.
    Steer,
    /// Cancel the in-flight turn and re-dispatch a merged prompt combining
    /// the original events with the new ones, framed as a **supersede** (the
    /// new request replaces the old), for ANY new @mention.
    /// Requires DedupMode::Queue.
    Interrupt,
    /// Cancel the in-flight turn only when the new @mention is from the agent
    /// owner (resolved via owner_cache). All other authors queue normally.
    /// Requires DedupMode::Queue.
    #[value(name = "owner-interrupt")]
    OwnerInterrupt,
}

/// Inbound author gate: which authors' events the harness forwards to the agent.
///
/// - `owner-only` â€” only the agent's registered owner (default).
/// - `allowlist`  â€” owner + explicit pubkey list (`--respond-to-allowlist`).
/// - `anyone`     â€” all events forwarded (no author filtering).
/// - `nobody`     â€” all events dropped (proactive/heartbeat-only mode).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum RespondTo {
    #[default]
    OwnerOnly,
    Allowlist,
    Anyone,
    Nobody,
}

impl std::fmt::Display for RespondTo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OwnerOnly => f.write_str("owner-only"),
            Self::Allowlist => f.write_str("allowlist"),
            Self::Anyone => f.write_str("anyone"),
            Self::Nobody => f.write_str("nobody"),
        }
    }
}

/// Permission mode for agents that support `session/set_config_option` with
/// `configId: "mode"` (e.g. `claude-agent-acp`).
///
/// - `default` â€” agent's built-in behaviour (permission requests per tool call).
/// - `acceptEdits` â€” auto-approve file edits, still ask for other tools.
/// - `dontAsk` â€” never prompt; reject anything that would require permission.
/// - `plan` â€” planning-only mode (no tool execution).
#[derive(Debug, Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum PermissionMode {
    /// Agent default â€” permission requests per tool call.
    #[value(alias = "default")]
    Default,
    /// Auto-approve file edits, still ask for other tools.
    #[value(alias = "acceptEdits")]
    AcceptEdits,
    /// Never prompt; reject anything that would require permission.
    #[value(alias = "dontAsk")]
    DontAsk,
    /// Planning-only mode (no tool execution).
    #[value(alias = "plan")]
    Plan,
}

impl PermissionMode {
    /// Return the wire-format string sent to the agent via
    /// `session/set_config_option`.
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::DontAsk => "dontAsk",
            Self::Plan => "plan",
        }
    }

    /// Returns `true` when the mode is the agent's built-in default and
    /// therefore doesn't need to be explicitly set.
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire_str())
    }
}

/// CLI args for `buzz-acp models` â€” query available models from an agent.
///
/// This is a standalone `Parser` (not a subcommand variant) because the
/// `models` path must bypass `Config::from_cli()` entirely â€” no relay,
/// no private key, no harness setup.
#[derive(Debug, Parser)]
#[command(
    name = "buzz-acp models",
    about = "Query available models from the configured agent"
)]
pub struct ModelsArgs {
    /// Agent binary to spawn (e.g. "goose", "claude-agent-acp", "codex-acp").
    #[command(flatten)]
    pub agent: AuthAgentArgs,

    /// Output structured JSON instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

/// Shared agent-spawn flags for lightweight local ACP helper subcommands.
#[derive(Debug, Parser)]
pub struct AuthAgentArgs {
    /// Agent binary to spawn (e.g. "goose", "claude-agent-acp", "codex-acp").
    #[arg(long, env = "BUZZ_ACP_AGENT_COMMAND", default_value = "goose")]
    pub agent_command: String,

    /// Arguments passed to the agent binary.
    #[arg(
        long,
        env = "BUZZ_ACP_AGENT_ARGS",
        default_value = "acp",
        value_delimiter = ','
    )]
    pub agent_args: Vec<String>,
}

/// CLI args for `buzz-acp auth-methods` â€” query adapter-advertised login methods.
#[derive(Debug, Parser)]
#[command(
    name = "buzz-acp auth-methods",
    about = "Query adapter-advertised ACP authentication methods"
)]
pub struct AuthMethodsArgs {
    #[command(flatten)]
    pub agent: AuthAgentArgs,

    /// Output structured JSON instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

/// CLI args for `buzz-acp authenticate` â€” start an adapter-owned login flow.
#[derive(Debug, Parser)]
#[command(
    name = "buzz-acp authenticate",
    about = "Start an adapter-owned ACP authentication flow"
)]
pub struct AuthenticateArgs {
    #[command(flatten)]
    pub agent: AuthAgentArgs,

    /// Adapter-advertised auth method id to invoke.
    #[arg(long)]
    pub method_id: String,
}

#[derive(Debug, Parser)]
#[command(
    name = "buzz-acp",
    about = "ACP harness that bridges Pkzz events to AI agents"
)]
pub struct CliArgs {
    #[arg(long, env = "BUZZ_RELAY_URL", default_value = "ws://localhost:3000")]
    pub relay_url: String,

    #[arg(long, env = "BUZZ_PRIVATE_KEY", hide_env_values = true)]
    pub private_key: String,

    /// Agent owner pubkey (64-char hex). Used for --respond-to=owner-only gate.
    #[arg(long, env = "BUZZ_ACP_AGENT_OWNER")]
    pub agent_owner: Option<String>,

    #[arg(long, env = "BUZZ_ACP_AGENT_COMMAND", default_value = "goose")]
    pub agent_command: String,

    #[arg(
        long,
        env = "BUZZ_ACP_AGENT_ARGS",
        default_value = "acp",
        value_delimiter = ','
    )]
    pub agent_args: Vec<String>,

    #[arg(long, env = "BUZZ_ACP_MCP_COMMAND", default_value = "")]
    pub mcp_command: String,

    /// Idle timeout: max seconds of silence before killing a turn.
    /// Resets on any agent stdout activity.
    #[arg(long, env = "BUZZ_ACP_IDLE_TIMEOUT")]
    pub idle_timeout: Option<u64>,

    /// Absolute wall-clock cap per turn (safety valve).
    #[arg(long, env = "BUZZ_ACP_MAX_TURN_DURATION", default_value_t = DEFAULT_MAX_TURN_DURATION_SECS)]
    pub max_turn_duration: u64,

    /// Deprecated: alias for --idle-timeout. If both set, --idle-timeout wins.
    #[arg(long, env = "BUZZ_ACP_TURN_TIMEOUT", hide = true)]
    pub turn_timeout: Option<u64>,

    #[arg(
        long,
        env = "BUZZ_ACP_SYSTEM_PROMPT",
        conflicts_with = "system_prompt_file"
    )]
    pub system_prompt: Option<String>,

    #[arg(
        long,
        env = "BUZZ_ACP_SYSTEM_PROMPT_FILE",
        conflicts_with = "system_prompt"
    )]
    pub system_prompt_file: Option<PathBuf>,

    /// Number of parallel agent subprocesses.
    #[arg(long, env = "BUZZ_ACP_AGENTS", default_value_t = 1,
          value_parser = clap::value_parser!(u32).range(1..=32))]
    pub agents: u32,

    /// Seconds between heartbeat prompts. 0 = disabled.
    #[arg(long, env = "BUZZ_ACP_HEARTBEAT_INTERVAL", default_value_t = 0)]
    pub heartbeat_interval: u64,

    /// Seconds between per-turn liveness pings (the crash backstop signal â€”
    /// distinct from heartbeat self-prompting). 0 = disabled.
    #[arg(long, env = "BUZZ_ACP_TURN_LIVENESS_SECS", default_value_t = 10)]
    pub turn_liveness_secs: u64,

    /// Heartbeat prompt text. Conflicts with --heartbeat-prompt-file.
    #[arg(
        long,
        env = "BUZZ_ACP_HEARTBEAT_PROMPT",
        conflicts_with = "heartbeat_prompt_file"
    )]
    pub heartbeat_prompt: Option<String>,

    /// Read heartbeat prompt from file.
    #[arg(
        long,
        env = "BUZZ_ACP_HEARTBEAT_PROMPT_FILE",
        conflicts_with = "heartbeat_prompt"
    )]
    pub heartbeat_prompt_file: Option<PathBuf>,

    #[arg(long, env = "BUZZ_ACP_INITIAL_MESSAGE")]
    pub initial_message: Option<String>,

    #[arg(
        long,
        env = "BUZZ_ACP_SUBSCRIBE",
        default_value = "mentions",
        value_enum
    )]
    pub subscribe: SubscribeMode,

    #[arg(long, env = "BUZZ_ACP_KINDS", value_delimiter = ',')]
    pub kinds: Option<Vec<u32>>,

    #[arg(long, env = "BUZZ_ACP_CHANNELS", value_delimiter = ',')]
    pub channels: Option<Vec<String>>,

    #[arg(long, env = "BUZZ_ACP_NO_MENTION_FILTER")]
    pub no_mention_filter: bool,

    /// Engagement mode for the synthesized mentions-mode rule.
    ///
    /// `mentions` (default behavior when unset) requires a `p` tag; `thread`
    /// additionally continues conversations in threads the agent has posted
    /// in (guardrailed); `all` fires on everything in scope. Only consulted
    /// in `--subscribe mentions` mode: All mode is already unconditional and
    /// Config mode carries per-rule `engagement` fields. When unset, the
    /// legacy `--no-mention-filter` mapping applies.
    #[arg(long, env = "BUZZ_ACP_ENGAGEMENT", value_enum)]
    pub engagement: Option<crate::filter::EngagementMode>,

    /// Consecutive thread-engaged turns allowed per thread without an owner
    /// message or explicit @mention (loop brake for agent-to-agent chatter).
    #[arg(long, env = "BUZZ_ACP_MAX_AGENT_CHAIN", default_value_t = 3)]
    pub max_agent_chain: u32,

    /// Minimum seconds between thread-engaged (non-mention) turns per
    /// channel. Explicit mentions bypass this. 0 disables the cooldown.
    #[arg(long, env = "BUZZ_ACP_THREAD_ENGAGE_COOLDOWN", default_value_t = 15)]
    pub thread_engage_cooldown: u64,

    #[arg(long, env = "BUZZ_ACP_CONFIG", default_value = "./buzz-acp.toml")]
    pub config: PathBuf,

    /// Durable host-final delivery records. Desktop supplies a protected,
    /// pair-scoped app-data directory; direct harness use defaults beside this
    /// config file so delivery recovery is stable across process restarts.
    #[arg(long, env = "BUZZ_ACP_HOST_FINAL_OUTBOX_DIR")]
    pub host_final_outbox_dir: Option<PathBuf>,

    #[arg(long, env = "BUZZ_ACP_DEDUP", default_value = "queue", value_enum)]
    pub dedup: DedupMode,

    /// How to handle new @mentions while a turn is already in-flight.
    /// steer (default): cancel+re-prompt, framing the new mention as a message
    /// that arrived mid-task â€” the agent keeps working and weaves it in.
    /// queue: events wait until the current turn completes.
    /// interrupt: cancel+re-prompt framed as a supersede (new replaces old).
    /// owner-interrupt: interrupt only for the agent owner's mentions.
    #[arg(
        long,
        env = "BUZZ_ACP_MULTIPLE_EVENT_HANDLING",
        default_value = "steer",
        value_enum
    )]
    pub multiple_event_handling: MultipleEventHandling,

    #[arg(long, env = "BUZZ_ACP_NO_IGNORE_SELF")]
    pub no_ignore_self: bool,

    /// Maximum number of context messages to include for thread replies and DMs.
    /// Set to 0 to disable automatic context fetching. Max 100.
    #[arg(long, env = "BUZZ_ACP_CONTEXT_MESSAGE_LIMIT", default_value_t = 12,
          value_parser = clap::value_parser!(u32).range(0..=100))]
    pub context_message_limit: u32,

    /// Maximum turns per session before proactive rotation. 0 = disabled
    /// (rotate only on MaxTokens / MaxTurnRequests).
    #[arg(long, env = "BUZZ_ACP_MAX_TURNS_PER_SESSION", default_value_t = 0,
          value_parser = clap::value_parser!(u32))]
    pub max_turns_per_session: u32,

    /// Disable automatic presence (online/offline) status.
    #[arg(long, env = "BUZZ_ACP_NO_PRESENCE")]
    pub no_presence: bool,

    /// Disable typing indicators while agent is processing.
    #[arg(long, env = "BUZZ_ACP_NO_TYPING")]
    pub no_typing: bool,

    /// Enable NIP-AE agent core memory injection.
    ///
    /// Memory injection is on by default. When enabled, the harness
    /// fetches the agent's per-session core engram and renders it as an
    /// `[Agent Memory â€” core]` prompt section (or renders the onboarding nudge
    /// when the relay confirms no core engram exists). The `buzz mem` CLI
    /// and the relay's acceptance of kind:30174 engrams are unaffected âÛÍôÖÚ$z{-®éÜj×WfVçBÖ†æFÆ–ærfÆ–FF–öâ²FVfVÇB)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H  ¢5·FW7EĞ¢fâFW7Eö×VÇF—ÆUöWfVçEö†æFÆ–æuöFVfVÇEö—5÷7FVW"‚’°¢òò'6RÖ–æ–ÖÂ&r6WC²F†RFVfVÇBf÷"ÒÖ×VÇF—ÆRÖWfVçBÖ†æFÆ–æp¢òò×W7B&R7FVW&‡7FVW&–ær—2F†RFVfVÇBÖ–B×GW&âFVÆ—fW'’F‚’à¢ÆWB&w2Ò6Æ”&w3£§'6Uög&öÒ…²&'W§¢Ö7"Â"Ò×&—fFRÖ¶W’"Âb#"ç&WVBƒcB•Ò“°¢76W'EöW†&w2æ×VÇF—ÆUöWfVçEö†æFÆ–ærÂ×VÇF—ÆTWfVçD†æFÆ–æs£¥7FVW"“°¢òòFVGWFVfVÇB×W7B&VÖ–âVWVV6ò7FVW&–ærw2&WV—&VÖVçB—2ÖWBà¢76W'B†ÖF6†W2†&w2æFVGWÂFVGWÖöFS£¥VWVR’“°¢Ğ ¢5·FW7EĞ¢fâFW7E÷fÆ–FFU÷7FVW%÷&WV—&W5÷VWVUöFVGW‚’°¢òò7FVW"²G&÷—2&V¦V7FVB†G&–âv–æF÷rv÷VÆBG&÷WfVçG2’à¢ÆWBW'"ÒfÆ–FFUö×VÇF—ÆUöWfVçEö†æFÆ–ær„×VÇF—ÆTWfVçD†æFÆ–æs£¥7FVW"ÂFVGWÖöFS£¤G&÷¢çVçw&öW'"‚“°¢76W'B€¢W'"çFõ÷7G&–ær‚’æ6öçF–ç2‚'&WV—&W2"’À¢&W‡V7FVBFVGW×&WV—&VÖVçBW'&÷"Âv÷C¢¶W''Ò ¢“°¢òò7FVW"²VWVR—266WFVBà¢76W'B€¢fÆ–FFUö×VÇF—ÆUöWfVçEö†æFÆ–ær„×VÇF—ÆTWfVçD†æFÆ–æs£¥7FVW"ÂFVGWÖöFS£¥VWVR¢æ—5öö²‚¢“°¢Ğ ¢5·FW7EĞ¢fâFW7E÷fÆ–FFU÷VWVUö†æFÆ–æuöÆÆ÷w5öç•öFVGW‚’°¢òòF†RæöâÖ6æ6VÂVWVV†æFÆ–ær–×÷6W2æòFVGW6öç7G&–çBà¢76W'B€¢fÆ–FFUö×VÇF—ÆUöWfVçEö†æFÆ–ær„×VÇF—ÆTWfVçD†æFÆ–æs£¥VWVRÂFVGWÖöFS£¤G&÷’æ—5öö²‚¢“°¢76W'B€¢fÆ–FFUö×VÇF—ÆUöWfVçEö†æFÆ–ær„×VÇF—ÆTWfVçD†æFÆ–æs£¥VWVRÂFVGWÖöFS£¥VWVR¢æ—5öö²‚¢“°¢Ğ ¢5·FW7EĞ¢fâFW7E÷fÆ–FFUö–çFW''WEöÖöFW5÷7F–ÆÅ÷&WV—&U÷VWVR‚’°¢f÷"ÖöFR–â°¢×VÇF—ÆTWfVçD†æFÆ–æs£¤–çFW''WBÀ¢×VÇF—ÆTWfVçD†æFÆ–æs£¤÷væW$–çFW''WBÀ¢Ò°¢76W'B€¢fÆ–FFUö×VÇF—ÆUöWfVçEö†æFÆ–ær†ÖöFRÂFVGWÖöFS£¤G&÷’æ—5öW'"‚’À¢'¶ÖöFS£÷Ò²G&÷6†÷VÆB&R&V¦V7FVB ¢“°¢Ğ¢Ğ ¢òò)H)H–FÆRF–ÖV÷WB6öç7FçB²wV&B…"3“3R’)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H  ¢5·FW7EĞ¢fâFVfVÇEö–FÆU÷F–ÖV÷WEö—5ó“÷6V6öæG2‚’°¢òòÆö6²F†R6öç7FçBfÇVR6ò66–FVçFÂ6†ævW2&R6Vv‡Bà¢76W'EöW„DTdTÅEô”DÄUõD”ÔTõUEõ4T52Â““°¢Ğ ¢5·FW7EĞ¢fâ–FÆU÷F–ÖV÷WEö×W7Eö&UöÆW75÷F†åöÖ…÷GW&åöGW&F–öâ‚’°¢òòF†RwV&B–â6öæf–s£¦g&öÕö&w2&V¦V7G2–FÆRãÒÖ…÷GW&âà¢òòW†W&6—6RF†R6ÖRÆöv–3¢–b–FÆRãÒÖ…÷GW&âÂ—Bw2–çfÆ–Bà¢ÆWB–FÆRÒ3cScC°¢ÆWBÖ…÷GW&âÒ3cScC°¢76W'B€¢–FÆRãÒÖ…÷GW&âÀ¢'FW7B&V6öæF—F–öã¢–FÆR×W7B&RãÒÖ…÷GW&âFòG&–vvW"wV&B ¢“° ¢òòæBF†RfÆ–B66R†6öç7B76W'F–öâ6ò6Æ—’FöW6âwBfÆr—B“ ¢6öç7B°¢76W'B„DTdTÅEô”DÄUõD”ÔTõUEõ4T52ÂDTdTÅEôÔ…õEU$åôEU$D”ôåõ4T52“°¢Ğ¢Ğ ¢òòÒÒÒ%U¥¥ô5ôÄÄõtTEõ$U5ôäEõDòvFRÒÒĞ ¢fâ'6UöÆÆ÷vVE÷&W7öæE÷Fò‡&s¢e²g7G%Ò’Óâ&W7VÇCÄ†6…6WCÅ&W7öæEFóâÂ6öæf–tW'&÷#â°¢ÆWB×WB6WBÒ†6…6WC£¦æWr‚“°¢f÷"2–â&r°¢ÆWBÖöFRÒ&W7öæEFó£¦g&öÕ÷7G"‡2çG&–Ò‚’ÂG'VR’æÖöW'"‡Å÷Â°¢6öæf–tW'&÷#£¤6öæf–tf–ÆR†f÷&ÖB€¢&–çfÆ–BfÇVR–â%U¥¥ô5ôÄÄõtTEõ$U5ôäEõDó¢w·7ÒrÀ¢‡fÆ–BfÇVW3¢÷væW"ÖöæÇ’ÂÆÆ÷vÆ—7BÂç–öæRÂæö&öG’’ ¢’¢Ò“ó°¢6WBæ–ç6W'B†ÖöFR“°¢Ğ¢ö²‡6WB¢Ğ ¢fâ6†V6µöÆÆ÷vVE÷&W7öæE÷Fò€¢ÆÆ÷vVE÷&s¢e²g7G%ÒÀ¢&W7öæE÷Fó¢&W7öæEFòÀ¢’Óâ&W7VÇCÂ‚’Â6öæf–tW'&÷#â°¢ÆWB6WBÒ'6UöÆÆ÷vVE÷&W7öæE÷Fò†ÆÆ÷vVE÷&r“ó°¢–b6WBæ—5öV×G’‚’bb6WBæ6öçF–ç2‚g&W7öæE÷Fò’°¢&WGW&âW'"„6öæf–tW'&÷#£¤6öæf–tf–ÆR†f÷&ÖB€¢'&W7öæE÷Fòw·Òr—2æ÷BW&Ö—GFVBöâF†—2FWÆ÷–ÖVçBÀ¢„%U¥¥ô5ôÄÄõtTEõ$U5ôäEõDó×·Ò’"À¢&W7öæE÷FòÀ¢ÆÆ÷vVE÷&ræ¦ö–â‚"Â"¢’’“°¢Ğ¢ö²‚‚’¢Ğ ¢5·FW7EĞ¢fâÆÆ÷vVE÷&W7öæE÷Fõ÷&V¦V7G5öF—6ÆÆ÷vVEöÖöFR‚’°¢ÆWB&W7VÇBÒ6†V6µöÆÆ÷vVE÷&W7öæE÷Fò‚e²&÷væW"ÖöæÇ’"Â&ÆÆ÷vÆ—7B%ÒÂ&W7öæEFó£¤ç–öæR“°¢76W'B€¢&W7VÇBæ—5öW'"‚’À¢&ç–öæR6†÷VÆB&R&V¦V7FVBv†Vâæ÷B–âÆÆ÷vVB6WB ¢“°¢ÆWB×6rÒ&W7VÇBçVçw&öW'"‚’çFõ÷7G&–ær‚“°¢76W'B€¢×6ræ6öçF–ç2‚&æ÷BW&Ö—GFVB"’À¢&W'&÷"6†÷VÆBÖVçF–öâvæ÷BW&Ö—GFVBs¢¶×6wÒ ¢“°¢Ğ ¢5·FW7EĞ¢fâÆÆ÷vVE÷&W7öæE÷Fõö66WG5öÆÆ÷vVEöÖöFR‚’°¢ÆWB&W7VÇBÒ6†V6µöÆÆ÷vVE÷&W7öæE÷Fò‚e²&÷væW"ÖöæÇ’"Â&ÆÆ÷vÆ—7B%ÒÂ&W7öæEFó£¤÷væW$öæÇ’“°¢76W'B‡&W7VÇBæ—5öö²‚’Â&÷væW"ÖöæÇ’6†÷VÆB&R66WFVC¢·&W7VÇC£÷Ò"“°¢Ğ ¢5·FW7EĞ¢fâÆÆ÷vVE÷&W7öæE÷FõöV×G•öÆÆ÷w5öÆÂ‚’°¢òòæò&W7G&–7F–öâ(	Bç–öæR—266WFVBà¢ÆWB&W7VÇBÒ6†V6µöÆÆ÷vVE÷&W7öæE÷Fò‚eµÒÂ&W7öæEFó£¤ç–öæR“°¢76W'B€¢&W7VÇBæ—5öö²‚’À¢&V×G’ÆÆ÷vVB6WB6†÷VÆBW&Ö—Bç’ÖöFS¢·&W7VÇC£÷Ò ¢“°¢Ğ ¢5·FW7EĞ¢fâÆÆ÷vVE÷&W7öæE÷Fõ÷&V¦V7G5ö–çfÆ–EöÖöFU÷7G&–ær‚’°¢ÆWB&W7VÇBÒ'6UöÆÆ÷vVE÷&W7öæE÷Fò‚e²&÷væW"ÖöæÇ’"Â&&GfÇVR%Ò“°¢76W'B‡&W7VÇBæ—5öW'"‚’Â&–çfÆ–BÖöFR7G&–ær6†÷VÆB&R&V¦V7FVB"“°¢ÆWB×6rÒ&W7VÇBçVçw&öW'"‚’çFõ÷7G&–ær‚“°¢76W'B€¢×6ræ6öçF–ç2‚&–çfÆ–BfÇVR–â%U¥¥ô5ôÄÄõtTEõ$U5ôäEõDò"’À¢&W'&÷"6†÷VÆBæÖRF†RVçbf#¢¶×6wÒ ¢“°¢76W'B€¢×6ræ6öçF–ç2‚&&GfÇVR"’À¢&W'&÷"6†÷VÆBæÖRF†R&BfÇVS¢¶×6wÒ ¢“°¢Ğ ¢5·FW7EĞ¢fâÆÆ÷vVE÷&W7öæE÷Fõ÷7VÖÖ'•÷6†÷w5÷&W7G&–7F–öå÷v†Vå÷6WB‚’°¢ÆWB×WB6öæf–rÒFW7Eö6öæf–r…7V'67&–&TÖöFS£¤ÖVçF–öç2“°¢6öæf–ræÆÆ÷vVE÷&W7öæE÷FòÒfV2²&÷væW"ÖöæÇ’"çFõ÷7G&–ær‚’Â&ÆÆ÷vÆ—7B"çFõ÷7G&–ær‚•Ó°¢ÆWB2Ò6öæf–rç7VÖÖ'’‚“°¢76W'B€¢2æ6öçF–ç2‚&ÆÆ÷vVE÷&W7öæE÷FóÒ"’À¢'7VÖÖ'’6†÷VÆB–æ6ÇVFRÆÆ÷vVE÷&W7öæE÷Fòv†Vâ6WC¢·7Ò ¢“°¢Ğ ¢5·FW7EĞ¢fâÆÆ÷vVE÷&W7öæE÷Fõ÷7VÖÖ'•ööÖ—GFVE÷v†VåöV×G’‚’°¢ÆWB6öæf–rÒFW7Eö6öæf–r…7V'67&–&TÖöFS£¤ÖVçF–öç2“°¢ÆWB2Ò6öæf–rç7VÖÖ'’‚“°¢76W'B€¢2æ6öçF–ç2‚&ÆÆ÷vVE÷&W7öæE÷FóÒ"’À¢'7VÖÖ'’6†÷VÆBæ÷B–æ6ÇVFRÆÆ÷vVE÷&W7öæE÷Fòv†VâV×G“¢·7Ò ¢“°¢Ğ ¢5·FW7EĞ¢fâ†÷7Eöf–æÅö÷WF&÷…öFVfVÇEö—5÷7F&Ç•ö6öæf–uöF¦6VçB‚’°¢76W'EöW€¢FVfVÇEö†÷7Eöf–æÅö÷WF&÷…öF—"…Fƒ£¦æWr‚'7FFRövVçBçFöÖÂ"’’À¢F„'Vc£¦g&öÒ‚'7FFR"’æ¦ö–â‚&†÷7BÖf–æÂÖ÷WF&÷‚"¢“°¢76W'EöW€¢FVfVÇEö†÷7Eöf–æÅö÷WF&÷…öF—"…Fƒ£¦æWr‚&vVçBçFöÖÂ"’’À¢F„'Vc£¦g&öÒ‚"â"’æ¦ö–â‚&†÷7BÖf–æÂÖ÷WF&÷‚"¢“°¢Ğ ¢òòÒÒÒ–çFVw&F–öâFW7G3¢gVÆÂVçb×f"(i"6Æ”&w2(i"6öæf–s£¦g&öÕö&w2‚’F‚ÒÒĞ¢òğ¢òòF†W6RFW7G2W†W&6—6RF†R7GVÂv—&–æs¢%U¥¥ô5ôÄÄõtTEõ$U5ôäEõDò–âF†P¢òòVçf—&öæÖVçB6W6W26ÆFò÷VÆFR6Æ”&w3£¦ÆÆ÷vVE÷&W7öæE÷FòÂv†–6‚F†Và¢òòfÆ÷w2F‡&÷Vv‚6öæf–s£¦g&öÕö&w2‚’Fò&öGV6R6öæf–tW'&÷"â–bF†R5¶&r†Vçb•Ğ¢òòGG&–'WFR÷"f–VÆBæÖRvW&R&VÖ÷fVBÂF†W6RFW7G2v÷VÆBf–Âà¢òğ¢òòvR72F†RfÇVRf–F†R4Ä’fÆr†ÒÖÆÆ÷vVB×&W7öæB×Fö’&F†W"F†à¢òò7FC£¦Vçc£§6WE÷f"Fòfö–BFW7B×&ÆÆVÆ—6Ò&6W2öâ6†&VBVçb7FFRà¢òòF†RVçb×f"v—&–ær—26÷fW&VB'’F†R6Æ5¶&r†Vçb•ÒGG&–'WFR—G6VÆbà ¢òòÖ–æ–ÖÂfÆ–B&—fFR¶W’f÷"FW7BW6R‡6V7#Sf³66Æ"Ò’à¢6öç7BDU5Eõ$•dDUô´U“¢g7G"Ğ¢##° ¢5·FW7EĞ¢fâÆÆ÷vVE÷&W7öæE÷FõögVÆÅ÷F…÷&V¦V7G5öF—6ÆÆ÷vVEöÖöFR‚’°¢òòÒÖÆÆ÷vVB×&W7öæB×FóÖ÷væW"ÖöæÇ’ÆÆÆ÷vÆ—7B²Ò×&W7öæB×FóÖç–öæR(i"6öæf–tW'&÷ ¢ÆWB&w2Ò6Æ”&w3£§G'•÷'6Uög&öÒ…°¢&'W§¢Ö7"À¢"Ò×&—fFRÖ¶W’"À¢DU5Eõ$•dDUô´U’À¢"Ò×&W7öæB×Fò"À¢&ç–öæR"À¢"ÒÖÆÆ÷vVB×&W7öæB×Fò"À¢&÷væW"ÖöæÇ’ÆÆÆ÷vÆ—7B"À¢Ò¢æW‡V7B‚&6Æ6†÷VÆB'6R&w2"“°¢ÆWB&W7VÇBÒ6öæf–s£¦g&öÕö&w2†&w2“° ¢76W'B€¢&W7VÇBæ—5öW'"‚’À¢&g&öÕö&w26†÷VÆB&V¦V7B&W7öæE÷FóÖç–öæRv†Vâæ÷B–âÆÆ÷vVB6WB ¢“°¢ÆWB×6rÒ&W7VÇBçVçw&öW'"‚’çFõ÷7G&–ær‚“°¢76W'B€¢×6ræ6öçF–ç2‚&æ÷BW&Ö—GFVB"’À¢&W'&÷"6†÷VÆBÖVçF–öâvæ÷BW&Ö—GFVBs¢¶×6wÒ ¢“°¢76W'B€¢×6ræ6öçF–ç2‚&ç–öæR"’À¢&W'&÷"6†÷VÆBæÖRF†RF—6ÆÆ÷vVBÖöFS¢¶×6wÒ ¢“°¢Ğ ¢5·FW7EĞ¢fâÆÆ÷vVE÷&W7öæE÷FõögVÆÅ÷F…ö66WG5öÆÆ÷vVEöÖöFR‚’°¢òòÒÖÆÆ÷vVB×&W7öæB×FóÖ÷væW"ÖöæÇ’ÆÆÆ÷vÆ—7B²Ò×&W7öæB×FóÖ÷væW"ÖöæÇ’(i"ö°¢ÆWB&w2Ò6Æ”&w3£§G'•÷'6Uög&öÒ…°¢&'W§¢Ö7"À¢"Ò×&—fFRÖ¶W’"À¢DU5Eõ$•dDUô´U’À¢"Ò×&W7öæB×Fò"À¢&÷væW"ÖöæÇ’"À¢"ÒÖÆÆ÷vVB×&W7öæB×Fò"À¢&÷væW"ÖöæÇ’ÆÆÆ÷vÆ—7B"À¢Ò¢æW‡V7B‚&6Æ6†÷VÆB'6R&w2"“°¢ÆWB&W7VÇBÒ6öæf–s£¦g&öÕö&w2†&w2“° ¢76W'B€¢&W7VÇBæ—5öö²‚’À¢&g&öÕö&w26†÷VÆB66WB&W7öæE÷FóÖ÷væW"ÖöæÇ’v†Vâ–âÆÆ÷vVB6WC¢·&W7VÇC£÷Ò ¢“°¢Ğ ¢5·FW7EĞ¢fâÆÆ÷vVE÷&W7öæE÷FõögVÆÅ÷F…÷Vç6WEöÆÆ÷w5öÆÂ‚’°¢òòæòÒÖÆÆ÷vVB×&W7öæB×FòfÆr(i"ç–öæR—266WFVBà¢ÆWB&w2Ò6Æ”&w3£§G'•÷'6Uög&öÒ…°¢&'W§¢Ö7"À¢"Ò×&—fFRÖ¶W’"À¢DU5Eõ$•dDUô´U’À¢"Ò×&W7öæB×Fò"À¢&ç–öæR"À¢Ò¢æW‡V7B‚&6Æ6†÷VÆB'6R&w2"“°¢ÆWB&W7VÇBÒ6öæf–s£¦g&öÕö&w2†&w2“° ¢76W'B€¢&W7VÇBæ—5öö²‚’À¢&g&öÕö&w26†÷VÆB66WBç’ÖöFRv†VâÆÆ÷vVBÆ—7B—2Vç6WC¢·&W7VÇC£÷Ò ¢“°¢Ğ ¢òòÒÒÒÖ…÷GW&åöGW&F–öâ6V–Æ–ærvFRÒÒĞ ¢5·FW7EĞ¢fâÖ…÷GW&åöGW&F–öåöEö6V–Æ–æuö—5ö66WFVB‚’°¢ÆWB&w2Ò6Æ”&w3£§G'•÷'6Uög&öÒ…°¢&'W§¢Ö7"À¢"Ò×&—fFRÖ¶W’"À¢DU5Eõ$•dDUô´U’À¢"ÒÖÖ‚×GW&âÖGW&F–öâ"À¢dÔ…õEU$åôEU$D”ôåô4T”Ä”äuõ4T52çFõ÷7G&–ær‚’À¢Ò¢æW‡V7B‚&6Æ6†÷VÆB'6R&w2"“°¢ÆWB&W7VÇBÒ6öæf–s£¦g&öÕö&w2†&w2“° ¢76W'B€¢&W7VÇBæ—5öö²‚’À¢&g&öÕö&w26†÷VÆB66WBÖ…÷GW&åöGW&F–öâBF†R6V–Æ–æs¢·&W7VÇC£÷Ò ¢“°¢Ğ ¢5·FW7EĞ¢fâÖ…÷GW&åöGW&F–öåö&÷fUö6V–Æ–æuö—5÷&V¦V7FVB‚’°¢ÆWB÷fW"ÒÔ…õEU$åôEU$D”ôåô4T”Ä”äuõ4T52²°¢ÆWB&w2Ò6Æ”&w3£§G'•÷'6Uög&öÒ…°¢&'W§¢Ö7"À¢"Ò×&—fFRÖ¶W’"À¢DU5Eõ$•dDUô´U’À¢"ÒÖÖ‚×GW&âÖGW&F–öâ"À¢f÷fW"çFõ÷7G&–ær‚’À¢Ò¢æW‡V7B‚&6Æ6†÷VÆB'6R&w2"“°¢ÆWB&W7VÇBÒ6öæf–s£¦g&öÕö&w2†&w2“° ¢76W'B€¢&W7VÇBæ—5öW'"‚’À¢&g&öÕö&w26†÷VÆB&V¦V7BÖ…÷GW&åöGW&F–öâ&÷fRF†R6V–Æ–ær ¢“°¢ÆWB×6rÒ&W7VÇBçVçw&öW'"‚’çFõ÷7G&–ær‚“°¢76W'B€¢×6ræ6öçF–ç2‚&W†6VVG26V–Æ–ær"’À¢&W'&÷"6†÷VÆBÖVçF–öâvW†6VVG26V–Æ–ærs¢¶×6wÒ ¢“°¢Ğ ¢5·FW7EĞ¢fâÖ…÷GW&åöGW&F–öåö6V–Æ–æuö6ææ÷Eö÷fW&fÆ÷uö–åöfÆ–v‡EöFVFÆ–æR‚’°¢òòF†R–âÖfÆ–v‡BFVFÆ–æR—2Ö…÷GW&â²2'VffW"„”åôdÄ”t…EôDTDÄ”äUô%TddU%õ4T52’à¢òòfW&–g’F†BWfVâBF†R6V–Æ–ærÂF†—2FF—F–öâ6ææ÷B÷fW&fÆ÷rScBà¢6öç7B°¢76W'B„Ô…õEU$åôEU$D”ôåô4T”Ä”äuõ4T52ÂScC£¤Ô‚Ò“°¢Ğ¢Ğ ¢5·FW7EĞ¢fâ6æ—F—¦U÷6W76–öå÷F—FÆUö6öÆÆ6W5÷v†—FW76UöæE÷7G&—5ö6öçG&öÅö6†'2‚’°¢76W'EöW€¢6æ—F—¦U÷6W76–öå÷F—FÆR‚"f—§¥ÇEÇGF†UÆâ&÷EÇW³wÒ"’À¢6öÖR‚$f—§¢F†R&÷B"çFõ÷7G&–ær‚’¢“°¢Ğ ¢5·FW7EĞ¢fâ6æ—F—¦U÷6W76–öå÷F—FÆU÷&WGW&ç5öæöæU÷v†Våöæ÷F†–æu÷&–çF&ÆU÷&VÖ–ç2‚’°¢76W'EöW‡6æ—F—¦U÷6W76–öå÷F—FÆR‚"ÆåÇB"’ÂæöæR“°¢76W'EöW‡6æ—F—¦U÷6W76–öå÷F—FÆR‚""’ÂæöæR“°¢76W'EöW‡6æ—F—¦U÷6W76–öå÷F—FÆR‚%ÇW³ÕÇW³'Ò"’ÂæöæR“°¢Ğ ¢5·FW7EĞ¢fâ6æ—F—¦U÷6W76–öå÷F—FÆUö65öÆVæwF…÷v—F†÷WE÷7Æ—GF–æuö×VÇF–'—FUö6†'2‚’°¢ÆWB&rÒ%ÇW³cCGÒ"ç&WVB…4U54”ôåõD•DÄUôÔ…ô4„%2²“°¢ÆWBF—FÆRÒ6æ—F—¦U÷6W76–öå÷F—FÆR‚g&r’æW‡V7B‚&VÖö¦’F—FÆR7W'f—fW26æ—F—¦–ær"“°¢76W'EöW‡F—FÆRæ6†'2‚’æ6÷VçB‚’Â4U54”ôåõD•DÄUôÔ…ô4„%2“°¢76W'B‡F—FÆRæ6†'2‚’æÆÂ‡Æ7Â2ÓÒuÇW³cCGÒr’“°¢Ğ ¢5·FW7EĞ¢fâ6æ—F—¦U÷6W76–öå÷F—FÆUöFöW5öæ÷EöÆVfUö÷G&–Æ–æu÷76UögFW%÷F†Uö6‚’°¢òòF†R6ÆæG2Ö–B×v÷&BÂ6òG&–ÖÖ–ær×W7Bæ÷BÆVfRFævÆ–ær76Rà¢ÆWB&rÒf÷&ÖB‚'·ÒF–Â"Â&"ç&WVB…4U54”ôåõD•DÄUôÔ…ô4„%2Ò’“°¢ÆWBF—FÆRÒ6æ—F—¦U÷6W76–öå÷F—FÆR‚g&r’æW‡V7B‚'F—FÆR7W'f—fW26æ—F—¦–ær"“°¢76W'EöW‡F—FÆRÂ&"ç&WVB…4U54”ôåõD•DÄUôÔ…ô4„%2Ò’“°¢Ğ ¢5·FW7EĞ¢fâ6ö×÷6U÷6W76–öå÷F—FÆU÷VÆ–f–W5÷F†UövVçEöæÖU÷v—F…÷F†Uö6†ææVÂ‚’°¢76W'EöW€¢6ö×÷6U÷6W76–öå÷F—FÆR‚$f—§¢"Â6öÖR‚&'W§¢ÖFWb"’’À¢$f—§¢+r6'W§¢ÖFWb ¢“°¢Ğ ¢5·FW7EĞ¢fâ6ö×÷6U÷6W76–öå÷F—FÆUöfÆÇ5ö&6µ÷Fõö&&UövVçEöæÖU÷v—F†÷WEöö6†ææVÂ‚’°¢76W'EöW†6ö×÷6U÷6W76–öå÷F—FÆR‚$f—§¢"ÂæöæR’Â$f—§¢"“°¢76W'EöW†6ö×÷6U÷6W76–öå÷F—FÆR‚$f—§¢"Â6öÖR‚""’’Â$f—§¢"“°¢Ğ ¢5·FW7EĞ¢fâ6ö×÷6U÷6W76–öå÷F—FÆU÷G'Væ6FW5÷F†Uö6†ææVÅöæEö¶VW5÷F†UövVçEöæÖR‚’°¢ÆWB6†ææVÂÒ&2"ç&WVBƒ#“°¢ÆWBF—FÆRÒ6ö×÷6U÷6W76–öå÷F—FÆR‚$f—§¢"Â6öÖR‚f6†ææVÂ’“°¢76W'EöW‡F—FÆRæ6†'2‚’æ6÷VçB‚’Â4U54”ôåõD•DÄUôÔ…ô4„%2“°¢76W'B‡F—FÆRç7F'G5÷v—F‚‚$f—§¢+r62"’“°¢Ğ ¢5·FW7EĞ¢fâ6ö×÷6U÷6W76–öå÷F—FÆUöG&÷5÷F†Uö6†ææVÅ÷v†Vå÷F†UövVçEöæÖUöf–ÆÇ5÷F†Uö6‚’°¢ÆWBvVçBÒ&"ç&WVB…4U54”ôåõD•DÄUôÔ…ô4„%2“°¢76W'EöW†6ö×÷6U÷6W76–öå÷F—FÆR‚fvVçBÂ6öÖR‚&'W§¢ÖFWb"’’ÂvVçB“°¢Ğ ¢òòòWfW'’&rv†÷6RVçbf"æÖR6öçF–ç2´U’õ4T5$UBõDô´Tâõ55tõ$Bô5$TBôUD€¢òòò×W7B6WB†–FUöVçe÷fÇVW2ÒG'VVFò&WfVçB7&VFVçF–ÂÆV¶vR–âÒÖ†VÇà¢5·FW7EĞ¢fâ6V7&WEöVçeö&w5ö†–FU÷F†V—%÷fÇVW5ö–åö†VÇ‚’°¢W6R6Æ£¤6öÖÖæDf7F÷'“° ¢6öç7B4T5$UEõEDU$å3¢e²g7G%ÒÒe²$´U’"Â%4T5$UB"Â%Dô´Tâ"Â%55tõ$B"Â$5$TB"Â$UD‚%Ó° ¢ÆWB6ÖBÒ6Æ”&w3£¦6öÖÖæB‚“°¢ÆWBf–öÆF–öç3¢fV3Å7G&–æsâÒ6Ö@¢ævWEö&wVÖVçG2‚¢æf–ÇFW%öÖ‡Æ&wÂ°¢ÆWBVçeö¶W’Ò&rævWEöVçb‚“ó°¢ÆWBVçeöæÖRÒVçeö¶W’çFõ÷7G&–æuöÆ÷77’‚’çFõ÷WW&66R‚“°¢ÆWB—5÷6V7&WBÒ4T5$UEõEDU$å2æ—FW"‚’æç’‡ÇGÂVçeöæÖRæ6öçF–ç2‡B’“°¢–b—5÷6V7&WBbb&ræ—5ö†–FUöVçe÷fÇVW5÷6WB‚’°¢6öÖR†VçeöæÖR¢ÒVÇ6R°¢æöæP¢Ğ¢Ò¢æ6öÆÆV7B‚“° ¢76W'B€¢f–öÆF–öç2æ—5öV×G’‚’À¢$f÷VæB6V7&WBÖ&V&–ærVçb&w2v—F†÷WB†–FUöVçe÷fÇVW3×G'VRâÀ¢FB†–FUöVçe÷fÇVW2ÒG'VVFòV6ƒ¢·f–öÆF–öç3£÷Ò ¢“°¢Ğ§Ğ