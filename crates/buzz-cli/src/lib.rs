pub mod agent_management;
mod client;
mod commands;
mod error;
mod links;
mod validate;

use clap::{Parser, Subcommand};
use client::BuzzClient;
use error::CliError;
use nostr::Keys;
use uuid::Uuid;

/// Run the Pkzz CLI from raw arguments (including `argv[0]`).
///
/// Returns a process exit code (0 = success).
///
/// # Example
///
/// ```ignore
/// let code = buzz_cli::run_from_args(std::env::args()).await;
/// std::process::exit(code);
/// ```
pub async fn run_from_args<I, S>(args: I) -> i32
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
{
    // Install ring as the process-level rustls CryptoProvider. Required because the
    // release workflow builds all binaries in one cargo invocation, which unifies
    // features across the workspace and enables *both* ring (from buzz-acp/buzz-dev-mcp)
    // and aws-lc-rs (from reqwest's rustls feature via hyper-rustls). With both on,
    // rustls cannot auto-select a provider, and any code that reaches
    // ClientConfig::builder() â€” specifically the WSS path in publish_ephemeral_event
    // used by `agents draft-create`, `agents draft-update`, and `users set-presence`
    // â€” panics at rustls crypto/mod.rs. The `let _ =` swallow is intentional: when
    // buzz-dev-mcp delegates to run_from_args, it has already installed ring; the
    // double-install returns Err and is harmless.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            if e.use_stderr() {
                error::print_error(&CliError::Usage(e.to_string()));
                return 1;
            } else {
                // --help and --version: print normally (intentional human output)
                let _ = e.print();
                return 0;
            }
        }
    };
    match run(cli).await {
        Ok(()) => 0,
        Err(e) => {
            error::print_error(&e);
            error::exit_code(&e)
        }
    }
}

#[derive(Parser)]
#[command(
    name = "buzz",
    about = "Pkzz CLI â€” interact with a Pkzz relay",
    long_about = "\
Pkzz CLI â€” interact with a Pkzz relay

Configuration (flags override env vars):
  BUZZ_RELAY_URL     Relay base URL        [default: http://localhost:3000]
  BUZZ_PRIVATE_KEY   Nostr private key (hex or nsec)  [required]
  BUZZ_AUTH_TAG      NIP-OA auth tag JSON  [optional]

The 'pack' subcommand runs locally and does not require a relay connection.

Exit codes: 0=ok  1=bad input  2=relay/network error  3=auth error  4=other  5=write conflict
Errors are JSON on stderr: {\"error\": \"<category>\", \"message\": \"<detail>\"}"
)]
struct Cli {
    /// Relay URL (http:// or https://). Overrides BUZZ_RELAY_URL env var.
    #[arg(long, env = "BUZZ_RELAY_URL", default_value = "http://localhost:3000")]
    relay: String,

    /// Nostr private key (hex or nsec). This is the CLI's identity.
    #[arg(long, env = "BUZZ_PRIVATE_KEY", hide_env_values = true)]
    private_key: Option<String>,

    /// NIP-OA auth tag JSON (owner attestation). Injected into every signed event.
    #[arg(long, env = "BUZZ_AUTH_TAG", hide_env_values = true)]
    auth_tag: Option<String>,

    /// Output format: 'json' (default, full fields) or 'compact' (reduced fields).
    #[arg(long, value_enum, default_value = "json")]
    format: OutputFormat,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Clone, clap::ValueEnum)]
pub enum ChannelType {
    #[value(name = "stream")]
    Stream,
    #[value(name = "forum")]
    Forum,
}

impl std::fmt::Display for ChannelType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stream => write!(f, "stream"),
            Self::Forum => write!(f, "forum"),
        }
    }
}

#[derive(Clone, clap::ValueEnum)]
pub enum ChannelVisibility {
    #[value(name = "open")]
    Open,
    #[value(name = "private")]
    Private,
}

impl std::fmt::Display for ChannelVisibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open => write!(f, "open"),
            Self::Private => write!(f, "private"),
        }
    }
}

#[derive(Clone, clap::ValueEnum)]
pub enum PresenceStatus {
    #[value(name = "online")]
    Online,
    #[value(name = "away")]
    Away,
    #[value(name = "offline")]
    Offline,
}

#[derive(Clone, clap::ValueEnum)]
pub enum EmojiScope {
    #[value(name = "own")]
    Own,
    #[value(name = "workspace")]
    Workspace,
}

impl std::fmt::Display for PresenceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Online => write!(f, "online"),
            Self::Away => write!(f, "away"),
            Self::Offline => write!(f, "offline"),
        }
    }
}

/// Output format for read commands.
#[derive(Clone, clap::ValueEnum, Default)]
pub enum OutputFormat {
    /// Full normalized JSON (default)
    #[default]
    #[value(name = "json")]
    Json,
    /// Reduced fields for agent scanning
    #[value(name = "compact")]
    Compact,
}

#[derive(Subcommand)]
enum Cmd {
    /// Draft owner-reviewed agent creation and updates
    #[command(subcommand)]
    Agents(AgentsCmd),
    /// Send, read, search, and manage messages
    #[command(subcommand)]
    Messages(MessagesCmd),
    /// Create, configure, and manage channels
    #[command(subcommand)]
    Channels(ChannelsCmd),
    /// Get and set channel canvas documents
    #[command(subcommand)]
    Canvas(CanvasCmd),
    /// Add, remove, and list emoji reactions
    #[command(subcommand)]
    Reactions(ReactionsCmd),
    /// Manage your custom emoji set (workspace palette is the union of all members' sets)
    #[command(subcommand)]
    Emoji(EmojiCmd),
    /// List, open, and manage direct messages
    #[command(subcommand)]
    Dms(DmsCmd),
    /// Look up users and manage profiles and presence
    #[command(subcommand)]
    Users(UsersCmd),
    /// Create, trigger, and manage workflows
    #[command(subcommand)]
    Workflows(WorkflowsCmd),
    /// Read the activity feed
    #[command(subcommand)]
    Feed(FeedCmd),
    /// Publish notes and manage the social graph (NIP-01/02)
    #[command(subcommand)]
    Social(SocialCmd),
    /// Publish and edit long-form NIP-23 notes â€” team knowledge base
    #[command(subcommand)]
    Notes(NotesCmd),
    /// Announce and discover git repositories (NIP-34)
    #[command(subcommand)]
    Repos(ReposCmd),
    /// Create and manage multi-repo projects (NIP-MP)
    #[command(subcommand)]
    Projects(ProjectsCmd),
    /// Send, get, list, and set status on git patches (NIP-34)
    #[command(subcommand)]
    Patches(PatchesCmd),
    /// Create, get, list, and set status on git issues (NIP-34)
    #[command(subcommand)]
    Issues(IssuesCmd),
    /// Open, update, list, and set status on git pull requests (NIP-34)
    #[command(subcommand)]
    Pr(PrCmd),
    /// Upload and download relay Blossom media
    #[command(subcommand)]
    Media(MediaCmd),
    /// Upload files to the relay's Blossom store
    #[command(subcommand)]
    Upload(UploadCmd),
    /// Agent engram management â€” persistent memory per NIP-AE
    #[command(subcommand)]
    Mem(MemCmd),
    /// Persona pack operations (local, no relay connection needed)
    #[command(subcommand)]
    Pack(PackCmd),
    /// Community moderation â€” reports queue, bans, timeouts, audit trail
    #[command(subcommand)]
    Moderation(ModerationCmd),
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum RespondToArg {
    #[value(name = "owner-only")]
    OwnerOnly,
    #[value(name = "anyone")]
    Anyone,
}

impl RespondToArg {
    fn to_wire(self) -> String {
        match self {
            Self::OwnerOnly => "owner-only",
            Self::Anyone => "anyone",
        }
        .to_string()
    }
}

#[derive(Subcommand)]
pub enum AgentsCmd {
    /// Open a prefilled create-agent form in the owner's Pkzz Desktop
    DraftCreate {
        /// Current channel UUID; the new agent is added here after save
        #[arg(long)]
        channel: String,
        /// Proposed agent name
        #[arg(long)]
        display_name: String,
        /// Proposed instructions; use '-' to read from stdin
        #[arg(long)]
        system_prompt: String,
    },
    /// Open a prefilled edit-agent form in the owner's Pkzz Desktop
    DraftUpdate {
        /// Current channel UUID
        #[arg(long)]
        channel: String,
        /// Current name of the personal agent to update
        #[arg(long)]
        agent_name: String,
        #[arg(long)]
        display_name: Option<String>,
        /// Replacement instructions; use '-' to read from stdin
        #[arg(long)]
        system_prompt: Option<String>,
        #[arg(long)]
        runtime: Option<String>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, value_enum)]
        respond_to: Option<RespondToArg>,
    },
    /// Submit a NIP-IA archive request for an identity (kind 9035)
    #[command(
        after_help = "The relay chooses the consent path (self / admin / owner) from the \
submitted request; this command does not retry with a different shape.\n\n\
Suggested --reason codes (unknown values are allowed): rotated, retired, \
bot-rebuilt, left-organization, spam\n\n\
Archiving a third-party identity is a human owner/admin action: an agent \
running under BUZZ_AUTH_TAG signs as itself, so it can only ever satisfy \
the self path (target == signer) â€” not the owner-of-agent path for another \
identity.\n\n\
Examples:\n  \
buzz agents archive <PUBKEY> --reason retired\n  \
buzz agents archive <PUBKEY> --reason bot-rebuilt --replaced-by <NEW_PUBKEY>"
    )]
    Archive {
        /// Target identity pubkey (hex)
        target_pubkey: String,
        /// Machine-readable reason code, max 64 UTF-8 bytes
        #[arg(long)]
        reason: Option<String>,
        /// Rotation pointer pubkey (hex); must differ from the target
        #[arg(long)]
        replaced_by: Option<String>,
        /// Optional human-readable note (not parsed for authorization)
        #[arg(long, default_value = "")]
        content: String,
    },
    /// Submit a NIP-IA unarchive request for an identity (kind 9036)
    #[command(after_help = "Examples:\n  \
buzz agents unarchive <PUBKEY> --reason returned")]
    Unarchive {
        /// Target identity pubkey (hex)
        target_pubkey: String,
        /// Machine-readable reason code, max 64 UTF-8 bytes
        #[arg(long)]
        reason: Option<String>,
        /// Optional human-readable note (not parsed for authorization)
        #[arg(long, default_value = "")]
        content: String,
    },
    /// Read the relay's current NIP-IA archive snapshot (kind 13535)
    #[command(
        after_help = "Verifies the snapshot's NIP-11 `self` authorship, event id, signature, \
and NIP-70 `-` protection tag before trusting it. Any trust failure is a \
nonzero-exit error, never a false-empty success â€” this command's whole \
purpose is verification.\n\n\
Examples:\n  \
buzz agents archived"
    )]
    Archived,
}

#[derive(Subcommand)]
pub enum MessagesCmd {
    /// Send a message to a channel
    #[command(
        after_help = "Examples:\n  buzz messages send --channel <UUID> --content \"hello\"\n  buzz messages send --channel <UUID> --content \"@alice check this\"\n  echo \"hello from stdin\" | buzz messages send --channel <UUID> --content -"
    )]
    Send {
        /// Channel UUID (from 'buzz channels list')
        #[arg(long)]
        channel: String,
        /// Message text â€” supports @mentions and markdown. Use '-' to read from stdin.
        #[arg(long)]
        content: String,
        /// Nostr event kind (default: channel default)
        #[arg(long)]
        kind: Option<u16>,
        /// Event ID to reply to (creates a thread)
        #[arg(long)]
        reply_to: Option<String>,
        /// Also publish to the Nostr network
        #[arg(long, default_value_t = false)]
        broadcast: bool,
        /// Attach file(s) â€” uploads and includes as imeta tags
        #[arg(long = "file")]
        files: Vec<String>,
        /// Pubkey to mention (hex or npub; repeatable). Supplying any explicit identity permits unresolved or ambiguous @Name text as presentation-only; uniquely resolved member names still notify.
        #[arg(long = "mention")]
        mentions: Vec<String>,
        /// Mark this signed message as an OMPK execution request. Target an OMPK agent with the normal mention mechanism.
        #[arg(long, default_value_t = false)]
        ompk_execution: bool,
        /// Request an absolute OMPK session working directory. OMPK and the harness enforce the configured workspace policy.
        #[arg(long, requires = "ompk_execution")]
        ompk_cwd: Option<String>,
    },
    /// Send a code diff / patch to a channel
    SendDiff {
        /// Channel UUID
        #[arg(long)]
        channel: String,
        /// Diff/patch content (use '-' to read from stdin)
        #[arg(long)]
        diff: String,
        /// Repository URL (e.g. https://github.com/org/repo)
        #[arg(long)]
        repo: String,
        /// Commit SHA
        #[arg(long)]
        commit: String,
        /// Single file path within the repo
        #[arg(long)]
        file: Option<String>,
        /// Parent commit SHA for three-way diff context
        #[arg(long)]
        parent_commit: Option<String>,
        /// Source branch name
        #[arg(long)]
        source_branch: Option<String>,
        /// Target branch name
        #[arg(long)]
        target_branch: Option<String>,
        /// Pull request number
        #[arg(long)]
        pr: Option<u32>,
        /// Language hint (auto-detected from file extension if omitted)
        #[arg(long)]
        lang: Option<String>,
        /// Human-readable description of the change
        #[arg(long)]
        description: Option<String>,
        /// Event ID to reply to (creates a thread)
        #[arg(long)]
        reply_to: Option<String>,
    },
    /// Edit a previously sent message
    Edit {
        /// Event ID of the message to edit (64-char hex)
        #[arg(long)]
        event: String,
        /// New message content
        #[arg(long)]
        content: String,
    },
    /// Delete a message by event ID
    Delete {
        /// Event ID to delete (64-char hex)
        #[arg(long)]
        event: String,
        /// Optional moderation audit action UUID for the public tombstone
        #[arg(long)]
        action_id: Option<Uuid>,
        /// Optional machine-readable public reason code for the tombstone
        #[arg(long)]
        reason_code: Option<String>,
        /// Optional human-readable public reason for the tombstone
        #[arg(long)]
        public_reason: Option<String>,
    },
    /// Retrieve messages from a channel
   ×Ï<òÚ$z{-®éÜj×÷BFr"À¢%¶WF‚ÇFöòÆfWuÒ"À¢%¶Æ"Æ2ÆBÆUÒ"À¢"2%¶WF‚Â'V÷FVB"Ç‚Ç•Ò"2ÂòòV÷FR6†'2Óâæ÷BF†R6†÷'F†æ@¢%µÒ"À¢'µÂ&WF…Â#£Ò"À¢Ò°¢76W'EöW†æ÷&ÖÆ—¦UöWF…÷Fuö–çWB†v&&vR’Âv&&vRçG&–Ò‚’“°¢Ð¢Ð ¢òòò6Öö¶RFW7C¢4Ä’FVf–æ—F–öâ—2fÆ–BæB'6V&ÆRà¢5·FW7EÐ¢fâ6Æ•öFVf–æ—F–öåö—5÷fÆ–B‚’°¢6Æ“£¦6öÖÖæB‚’æFV'Vuö76W'B‚“°¢Ð ¢5·FW7EÐ¢fâÖW76vW5÷6VæE÷'6W5öö×µöW†V7WF–öåöÖWFFF‚’°¢5¶6fr‡v–æF÷w2•Ð¢6öç7B5tC¢g7G"Ò"$3¥Çv÷&·76UÇ&Wò#°¢5¶6fr†æ÷B‡v–æF÷w2’•Ð¢6öç7B5tC¢g7G"Ò"÷v÷&·76R÷&Wò#° ¢ÆWB6Æ’Ò6Æ“£§G'•÷'6Uög&öÒ…°¢&'W§¢"À¢&ÖW76vW2"À¢'6VæB"À¢"ÒÖ6†ææVÂ"À¢#ÓÓÓÓ"À¢"ÒÖ6öçFVçB"À¢&&÷VæFVBF6²"À¢"ÒÖö×²ÖW†V7WF–öâ"À¢"ÒÖö×²Ö7vB"À¢5tBÀ¢Ò¢æW‡V7B‚'fÆ–BôÕ²W†V7WF–öâ&WVW7B"“°¢ÆWB6ÖC£¤ÖW76vW2„ÖW76vW46ÖC£¥6VæB°¢ö×µöW†V7WF–öâÀ¢ö×µö7vBÀ¢âà¢Ò’Ò6Æ’æ6öÖÖæ@¢VÇ6R°¢æ–2‚&W‡V7FVBÖW76vW26VæB"“°¢Ó°¢76W'B†ö×µöW†V7WF–öâ“°¢76W'EöW†ö×µö7vBæ5öFW&Vb‚’Â6öÖR„5tB’“°¢Ð ¢5·FW7EÐ¢fâÖW76vW5÷6VæE÷&V¦V7G5öö×µö7vE÷v—F†÷WEöW†V7WF–öåö–çFVçB‚’°¢76W'B„6Æ“£§G'•÷'6Uög&öÒ…°¢&'W§¢"À¢&ÖW76vW2"À¢'6VæB"À¢"ÒÖ6†ææVÂ"À¢#ÓÓÓÓ"À¢"ÒÖ6öçFVçB"À¢&&÷VæFVBF6²"À¢"ÒÖö×²Ö7vB"À¢"÷v÷&·76R÷&Wò"À¢Ò¢æ—5öW'"‚’“°¢Ð ¢5·FW7EÐ¢fâ6WE÷7FGW5ö6ÆV%÷&V¦V7G5÷FW‡EöæEöVÖö¦’‚’°¢f÷"W‡G&–âµ²"Ò×FW‡B"Â&'W7’%ÒÂ²"ÒÖVÖö¦’"Â/	øëb%ÕÒ°¢ÆWB&w2Ò²&'W§¢"Â'W6W'2"Â'6WB×7FGW2"Â"ÒÖ6ÆV"%Ð¢æ–çFõö—FW"‚¢æ6†–â†W‡G&“°¢76W'B€¢6Æ“£§G'•÷'6Uög&öÒ†&w2’æ—5öW'"‚’À¢"ÒÖ6ÆV"×W7B6öæfÆ–7Bv—F‚·Ò"À¢W‡G&³Ð¢“°¢Ð¢Ð ¢5·FW7EÐ¢fâ6WE÷7FGW5÷&WV—&W5÷FW‡Eö÷%ö6ÆV"‚’°¢76W'B„6Æ“£§G'•÷'6Uög&öÒ…²&'W§¢"Â'W6W'2"Â'6WB×7FGW2%Ò’æ—5öW'"‚’“°¢76W'B€¢6Æ“£§G'•÷'6Uög&öÒ…²&'W§¢"Â'W6W'2"Â'6WB×7FGW2"Â"ÒÖVÖö¦’"Â/	øëb%Ò’æ—5öW'"‚’À¢"ÒÖVÖö¦’ÆöæR×W7Bæ÷B–×Ç’7FGW2 ¢“°¢76W'B„6Æ“£§G'•÷'6Uög&öÒ…²&'W§¢"Â'W6W'2"Â'6WB×7FGW2"Â"ÒÖ6ÆV"%Ò’æ—5öö²‚’“°¢Ð ¢5·FW7EÐ¢fâ6öÖÖæEö–çfVçF÷'•ö—5÷7F&ÆR‚’°¢ÆWBW‡V7FVEöw&÷W3¢fV3Âg7G#âÒfV2°¢&vVçG2"À¢&6çf2"À¢&6†ææVÇ2"À¢&F×2"À¢&VÖö¦’"À¢&fVVB"À¢&—77VW2"À¢&ÖVF–"À¢&ÖVÒ"À¢&ÖW76vW2"À¢&ÖöFW&F–öâ"À¢&æ÷FW2"À¢'6²"À¢'F6†W2"À¢'""À¢'&ö¦V7G2"À¢'&V7F–öç2"À¢'&W÷2"À¢'6ö6–Â"À¢'WÆöB"À¢'W6W'2"À¢'v÷&¶fÆ÷w2"À¢Ó° ¢ÆWB6ÖBÒ6Æ“£¦6öÖÖæB‚“°¢ÆWB×WB7GVÃ¢fV3Å7G&–æsâÒ6Ö@¢ævWE÷7V&6öÖÖæG2‚¢æÖ‡Ç7Â2ævWEöæÖR‚’çFõ÷7G&–ær‚’¢æf–ÇFW"‡ÆçÂâÒ&†VÇ"¢æ6öÆÆV7B‚“°¢7GVÂç6÷'B‚“° ¢76W'EöW€¢7GVÂæÆVâ‚’À¢W‡V7FVEöw&÷W2æÆVâ‚’À¢$W‡V7FVB·Òw&÷W2Âv÷B·Òâ7GVÃ¢³£÷Ò"À¢W‡V7FVEöw&÷W2æÆVâ‚’À¢7GVÂæÆVâ‚’À¢7GVÀ¢“°¢76W'EöW€¢7GVÂÂW‡V7FVEöw&÷W2À¢$6öÖÖæBw&÷W–çfVçF÷'’G&–gBFWFV7FVB ¢“°¢Ð ¢5·FW7EÐ¢fâ7V&6öÖÖæEöæÖW5ö&U÷7F&ÆR‚’°¢fâæÖW2†6ÖC¢f6Æ£¤6öÖÖæBÂw&÷W¢g7G"’ÓâfV3Å7G&–æsâ°¢ÆWBw&÷Wö6ÖBÒ6Ö@¢ævWE÷7V&6öÖÖæG2‚¢æf–æB‡Ç7Â2ævWEöæÖR‚’ÓÒw&÷W¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚&w&÷Ww·Òræ÷Bf÷VæB"Âw&÷W’“°¢ÆWB×WBæÖW3¢fV3Å7G&–æsâÒw&÷Wö6Ö@¢ævWE÷7V&6öÖÖæG2‚¢æÖ‡Ç7Â2ævWEöæÖR‚’çFõ÷7G&–ær‚’¢æf–ÇFW"‡ÆçÂâÒ&†VÇ"¢æ6öÆÆV7B‚“°¢æÖW2ç6÷'B‚“°¢æÖW0¢Ð ¢ÆWB6ÖBÒ6Æ“£¦6öÖÖæB‚“°¢76W'EöW€¢æÖW2‚f6ÖBÂ&vVçG2"’À¢fV2°¢&&6†—fR"À¢&&6†—fVB"À¢&G&gBÖ7&VFR"À¢&G&gB×WFFR"À¢'Væ&6†—fR ¢Ð¢“°¢76W'EöW€¢æÖW2‚f6ÖBÂ&ÖW76vW2"’À¢fV2°¢&FVÆWFR"À¢&VF—B"À¢&vWB"À¢'6V&6‚"À¢'6VæB"À¢'6VæBÖF–fb"À¢'F‡&VB"À¢'f÷FR ¢Ð¢“°¢76W'EöW€¢æÖW2‚f6ÖBÂ&6†ææVÇ2"’À¢fV2°¢&FBÖÖVÖ&W""À¢&&6†—fR"À¢&7&VFR"À¢&FVÆWFR"À¢&vWB"À¢&¦ö–â"À¢&ÆVfR"À¢&Æ—7B"À¢&ÖVÖ&W'2"À¢'W'÷6R"À¢'&VÖ÷fRÖÖVÖ&W""À¢'6V&6‚"À¢'6WBÖFB×öÆ–7’"À¢'F÷–2"À¢'Væ&6†—fR"À¢'WFFR ¢Ð¢“°¢76W'EöW†æÖW2‚f6ÖBÂ&6çf2"’ÂfV2²&vWB"Â'6WB%Ò“°¢76W'EöW†æÖW2‚f6ÖBÂ'&V7F–öç2"’ÂfV2²&FB"Â&vWB"Â'&VÖ÷fR%Ò“°¢76W'EöW€¢æÖW2‚f6ÖBÂ&VÖö¦’"’À¢fV2²&W‡÷'B"Â&–×÷'B"Â&Æ—7B"Â'&Ò"Â'6WB%Ð¢“°¢76W'EöW€¢æÖW2‚f6ÖBÂ&F×2"’À¢fV2²&FBÖÖVÖ&W""Â&†–FR"Â&Æ—7B"Â&÷Vâ%Ð¢“°¢76W'EöW€¢æÖW2‚f6ÖBÂ'W6W'2"’À¢fV2°¢&vWB"À¢'&W6Væ6R"À¢'6WB×&W6Væ6R"À¢'6WB×&öf–ÆR"À¢'6WB×7FGW2 ¢Ð¢“°¢76W'EöW€¢æÖW2‚f6ÖBÂ'v÷&¶fÆ÷w2"’À¢fV2²&&÷fR"Â&7&VFR"Â&FVÆWFR"Â&vWB"Â&Æ—7B"Â''Vç2"Â'G&–vvW""Â'WFFR%Ð¢“°¢76W'EöW†æÖW2‚f6ÖBÂ&fVVB"’ÂfV2²&vWB%Ò“°¢76W'EöW€¢æÖW2‚f6ÖBÂ'6ö6–Â"’À¢fV2°¢&6öçF7G2"À¢&WfVçB"À¢&Æ—7B"À¢&æ÷FW2"À¢'V&Æ—6‚"À¢'6WBÖ6öçF7G2"À¢'6WBÖÆ—7B ¢Ð¢“°¢76W'EöW€¢æÖW2‚f6ÖBÂ'&W÷2"’À¢fV2²&&–æB"Â&7&VFR"Â&vWB"Â&Æ—7B"Â'&÷FV7B%Ð¢“°¢ÆWB&W÷2Ò6Ö@¢ævWE÷7V&6öÖÖæG2‚¢æf–æB‡Ç7V&6öÖÖæGÂ7V&6öÖÖæBævWEöæÖR‚’ÓÒ'&W÷2"¢æW‡V7B‚'&W÷26öÖÖæB"“°¢ÆWB&÷FV7BÒ&W÷0¢ævWE÷7V&6öÖÖæG2‚¢æf–æB‡Ç7V&6öÖÖæGÂ7V&6öÖÖæBævWEöæÖR‚’ÓÒ'&÷FV7B"¢æW‡V7B‚'&W÷2&÷FV7B6öÖÖæB"“°¢ÆWB×WB&÷FV7EöæÖW3¢fV3Å7G&–æsâÒ&÷FV7@¢ævWE÷7V&6öÖÖæG2‚¢æÖ‡Ç7V&6öÖÖæGÂ7V&6öÖÖæBævWEöæÖR‚’çFõ÷7G&–ær‚’¢æf–ÇFW"‡ÆæÖWÂæÖRÒ&†VÇ"¢æ6öÆÆV7B‚“°¢&÷FV7EöæÖW2ç6÷'B‚“°¢76W'EöW‡&÷FV7EöæÖW2ÂfV2²&Æ—7B"Â'&VÖ÷fR"Â'6WB%Ò“°¢76W'EöW€¢æÖW2‚f6ÖBÂ'""’À¢fV2²&vWB"Â&Æ—7B"Â&÷Vâ"Â'7FGW2"Â'WFFR%Ð¢“°¢76W'EöW€¢æÖW2‚f6ÖBÂ'F6†W2"’À¢fV2²&vWB"Â&Æ—7B"Â'6VæB"Â'7FGW2%Ð¢“°¢76W'EöW€¢æÖW2‚f6ÖBÂ'&ö¦V7G2"’À¢fV2°¢&FB×&Wò"À¢&7&VFR"À¢&FVÆWFR"À¢&vWB"À¢&Æ—7B"À¢'&VÖ÷fR×&Wò"À¢'WFFR ¢Ð¢“°¢76W'EöW€¢æÖW2‚f6ÖBÂ&—77VW2"’À¢fV2²&7&VFR"Â&vWB"Â&Æ—7B"Â'7FGW2%Ð¢“°¢76W'EöW†æÖW2‚f6ÖBÂ&ÖVF–"’ÂfV2²&vWB%Ò“°¢76W'EöW†æÖW2‚f6ÖBÂ'WÆöB"’ÂfV2²&f–ÆR%Ò“°¢76W'EöW†æÖW2‚f6ÖBÂ'6²"’ÂfV2²&–ç7V7B"Â'fÆ–FFR%Ò“°¢76W'EöW€¢æÖW2‚f6ÖBÂ&ÖöFW&F–öâ"’À¢fV2°¢&VF—B"À¢&&â"À¢'&W÷'G2"À¢'&W6öÇfR"À¢'&W7G&–7FVB"À¢'F–ÖV÷WB"À¢'Væ&â"À¢'VçF–ÖV÷WB ¢Ð¢“°¢Ð ¢5·FW7EÐ¢fâ7V&6öÖÖæEö6÷VçG5ö&U÷7F&ÆR‚’°¢ÆWBW‡V7FVC¢fV3Â‚g7G"ÂW6—¦R“âÒfV2°¢‚&vVçG2"ÂR’À¢‚&6çf2"Â"’À¢‚&6†ææVÇ2"Âb’À¢‚&F×2"ÂB’À¢‚&VÖö¦’"ÂR’À¢‚&fVVB"Â’À¢‚&—77VW2"ÂB’À¢‚&ÖVF–"Â’À¢‚&ÖW76vW2"Â‚’À¢‚'6²"Â"’À¢‚'F6†W2"ÂB’À¢‚'""ÂR’À¢‚'&ö¦V7G2"Âr’À¢‚'&V7F–öç2"Â2’À¢‚'&W÷2"ÂR’À¢‚'6ö6–Â"Âr’À¢‚'WÆöB"Â’À¢‚'W6W'2"ÂR’À¢‚'v÷&¶fÆ÷w2"Â‚’À¢Ó° ¢ÆWB6ÖBÒ6Æ“£¦6öÖÖæB‚“°¢f÷"†w&÷WöæÖRÂW‡V7FVEö6÷VçB’–âfW‡V7FVB°¢ÆWBw&÷WÒ6Ö@¢ævWE÷7V&6öÖÖæG2‚¢æf–æB‡Ç7Â2ævWEöæÖR‚’ÓÒ¦w&÷WöæÖR¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚&w&÷Ww·Òræ÷Bf÷VæB"Âw&÷WöæÖR’“°¢ÆWB7GVÅö6÷VçBÒw&÷W ¢ævWE÷7V&6öÖÖæG2‚¢æf–ÇFW"‡Ç7Â2ævWEöæÖR‚’Ò&†VÇ"¢æ6÷VçB‚“°¢76W'EöW€¢7GVÅö6÷VçBÂ¦W‡V7FVEö6÷VçBÀ¢$w&÷Ww·Òs¢W‡V7FVB·Ò7V&6öÖÖæG2Âv÷B·Ò"À¢w&÷WöæÖRÂW‡V7FVEö6÷VçBÂ7GVÅö6÷Vç@¢“°¢Ð¢Ð ¢òòò6öÆÆV7BÆÂ&w2‡&V7W'6–ær–çFò7V&6öÖÖæG2’v†÷6RVçbf"æÖRÆöö·0¢òòòÆ–¶R7&VFVçF–Â'WBFöW2äõB†fR†–FUöVçe÷fÇVW66WBà¢fâ6öÆÆV7E÷Væ†–FFVå÷6V7&WEö&w2†6ÖC¢f6Æ£¤6öÖÖæB’ÓâfV3Â…7G&–ærÂ7G&–ær“â°¢6öç7B4T5$UEõEDU$å3¢e²g7G%ÒÒe²$´U’"Â%4T5$UB"Â%Dô´Tâ"Â%55tõ$B"Â$5$TB"Â$UD‚%Ó° ¢ÆWB×WBf–öÆF–öç3¢fV3Â…7G&–ærÂ7G&–ær“âÒfV3£¦æWr‚“° ¢f÷"&r–â6ÖBævWEö&wVÖVçG2‚’°¢–bÆWB6öÖR†Vçeö¶W’’Ò&rævWEöVçb‚’°¢ÆWBVçeöæÖRÒVçeö¶W’çFõ÷7G&–æuöÆ÷77’‚’çFõ÷WW&66R‚“°¢ÆWB—5÷6V7&WBÒ4T5$UEõEDU$å2æ—FW"‚’æç’‡ÇGÂVçeöæÖRæ6öçF–ç2‡B’“°¢–b—5÷6V7&WBbb&ræ—5ö†–FUöVçe÷fÇVW5÷6WB‚’°¢f–öÆF–öç2çW6‚‚†6ÖBævWEöæÖR‚’çFõ÷7G&–ær‚’ÂVçeöæÖR’“°¢Ð¢Ð¢Ð ¢f÷"7V"–â6ÖBævWE÷7V&6öÖÖæG2‚’°¢f–öÆF–öç2æW‡FVæB†6öÆÆV7E÷Væ†–FFVå÷6V7&WEö&w2‡7V"’“°¢Ð ¢f–öÆF–öç0¢Ð ¢òòòWfW'’&rv†÷6RVçbf"æÖR6öçF–ç2´U’õ4T5$UBõDô´Tâõ55tõ$Bô5$TBôUD€¢òòò×W7B6WB†–FUöVçe÷fÇVW2ÒG'VVFò&WfVçB7&VFVçF–ÂÆV¶vR–âÒÖ†VÇà¢5·FW7EÐ¢fâ6V7&WEöVçeö&w5ö†–FU÷F†V—%÷fÇVW5ö–åö†VÇ‚’°¢ÆWB6ÖBÒ6Æ“£¦6öÖÖæB‚“°¢ÆWBf–öÆF–öç2Ò6öÆÆV7E÷Væ†–FFVå÷6V7&WEö&w2‚f6ÖB“°¢76W'B€¢f–öÆF–öç2æ—5öV×G’‚’À¢$f÷VæB6V7&WBÖ&V&–ærVçb&w2v—F†÷WB†–FUöVçe÷fÇVW3×G'VRâÀ¢FB†–FUöVçe÷fÇVW2ÒG'VVFòV6ƒ¥Æç·Ò"À¢f–öÆF–öç0¢æ—FW"‚¢æÖ‡Â†6ÖBÂVçb—Âf÷&ÖB‚"6öÖÖæC×¶6ÖC£÷ÒVçc×¶Vçc£÷Ò"’¢æ6öÆÆV7C££ÅfV3Åóãâ‚¢æ¦ö–â‚%Æâ"¢“°¢Ð ¢òò)H)H&ö¦V7G2WFFR×WFF–öâw&÷W)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H  ¢òòò×VÇF—ÆR–æFWVæFVçBf–VÆG2×W7B&R66WFVB–âF†R6ÖR–çfö6F–öâà¢5·FW7EÐ¢fâ&ö¦V7G5÷WFFUö×VÇF•öf–VÆEö—5ö66WFVB‚’°¢76W'B€¢6Æ“£§G'•÷'6Uög&öÒ…°¢&'W§¢"À¢'&ö¦V7G2"À¢'WFFR"À¢&×’×6ÇVr"À¢"ÒÖæÖR"À¢%‚"À¢"ÒÖFW67&—F–öâ"À¢%’"À¢Ò¢æ—5öö²‚’À¢"ÒÖæÖRæBÒÖFW67&—F–öâFövWF†W"×W7B&R66WFVB ¢“°¢Ð ¢òòò6WGFW"f÷"öæRf–VÆBæB6ÆV&W"f÷"F–ffW&VçBf–VÆB×W7B&R66WFVBà¢5·FW7EÐ¢fâ&ö¦V7G5÷WFFU÷6WGFW%÷v—F…ö÷F†W%ö6ÆV&W%ö—5ö66WFVB‚’°¢76W'B€¢6Æ“£§G'•÷'6Uög&öÒ…°¢&'W§¢"À¢'&ö¦V7G2"À¢'WFFR"À¢&×’×6ÇVr"À¢"ÒÖæÖR"À¢%‚"À¢"ÒÖ6ÆV"ÖFW67&—F–öâ"À¢Ò¢æ—5öö²‚’À¢"ÒÖæÖRv—F‚ÒÖ6ÆV"ÖFW67&—F–öâ×W7B&R66WFVB ¢“°¢Ð ¢òòò6WGFW"æB—G2÷vâ6ÆV&W"&R×WGVÆÇ’W†6ÇW6—fR(	B6Æ×W7B&V¦V7BF†—2à¢5·FW7EÐ¢fâ&ö¦V7G5÷WFFU÷6WGFW%÷v—F…ö÷våö6ÆV&W%ö—5÷&V¦V7FVB‚’°¢76W'B€¢6Æ“£§G'•÷'6Uög&öÒ…°¢&'W§¢"À¢'&ö¦V7G2"À¢'WFFR"À¢&×’×6ÇVr"À¢"ÒÖæÖR"À¢%‚"À¢"ÒÖ6ÆV"ÖæÖR"À¢Ò¢æ—5öW'"‚’À¢"ÒÖæÖRæBÒÖ6ÆV"ÖæÖRFövWF†W"×W7B&R&V¦V7FVB'’6Æ ¢“°¢Ð ¢òòò&÷f–F–æræò×WFF–öâ÷F–öç2BÆÂ×W7B&R&V¦V7FVB'’6Æ‡&WV—&VBw&÷W’à¢5·FW7EÐ¢fâ&ö¦V7G5÷WFFUöæõö×WFF–öåö—5÷&V¦V7FVEö'•ö6Æ‚’°¢òòv—F†÷WB7&VFVçF–Ç2ÂfÆ–B'6Rv÷VÆB&V6‚WF†VçF–6F–öâæBf–À¢òòv—F‚WF…öW'&÷"(	B'WB6ÆÖÆWfVÂ&V¦V7F–öâ†Vç2&Vf÷&Rç’’ôòà¢òòvRfW&–g’—Bw26ÆW'&÷"†æ÷B§W7Bç’W'&÷"’'’6†V6¶–ærF†RW'&÷ ¢òò¶–æB—2æ÷B'VçF–ÖRöWF‚f–ÇW&R(	B6Æ“£§G'•÷'6Uög&öÒ&WGW&ç2W' ¢òò–ÖÖVF–FVÇ’f÷"&wVÖVçBf–öÆF–öç2à¢76W'B€¢6Æ“£§G'•÷'6Uög&öÒ…²&'W§¢"Â'&ö¦V7G2"Â'WFFR"Â&×’×6ÇVr%Ò’æ—5öW'"‚’À¢'WFFRv—F‚æò6WGFW'2÷"6ÆV&W'2×W7B&R&V¦V7FVBB'6RF–ÖR ¢“°¢Ð ¢òòòâVç&V6övæ—6VBf—6–&–Æ—G’Fö¶Vâ×W7B&R&V¦V7FVB'’6Æ&Vf÷&Rç’’ôòà¢5·FW7EÐ¢fâ&ö¦V7G5ö7&VFUö–çfÆ–E÷f—6–&–Æ—G•ö—5÷&V¦V7FVEö'•ö6Æ‚’°¢76W'B€¢6Æ“£§G'•÷'6Uög&öÒ…°¢&'W§¢"À¢'&ö¦V7G2"À¢&7&VFR"À¢&×’×6ÇVr"À¢"Ò×&Wò"À¢&'W§¢"À¢"Ò×f—6–&–Æ—G’"À¢&6†'G&WW6R"À¢Ò¢æ—5öW'"‚’À¢"Ò×f—6–&–Æ—G’6†'G&WW6R×W7B&R&V¦V7FVBB'6RF–ÖR ¢“°¢Ð ¢òòòâVç&V6övæ—6VBf—6–&–Æ—G’Fö¶VâöâWFFR×W7B&R&V¦V7FVB'’6Æ&Vf÷&Rç’’ôòà¢5·FW7EÐ¢fâ&ö¦V7G5÷WFFUö–çfÆ–E÷f—6–&–Æ—G•ö—5÷&V¦V7FVEö'•ö6Æ‚’°¢76W'B€¢6Æ“£§G'•÷'6Uög&öÒ…°¢&'W§¢"À¢'&ö¦V7G2"À¢'WFFR"À¢&×’×6ÇVr"À¢"Ò×f—6–&–Æ—G’"À¢&6†'G&WW6R"À¢Ò¢æ—5öW'"‚’À¢"Ò×f—6–&–Æ—G’6†'G&WW6RöâWFFR×W7B&R&V¦V7FVBB'6RF–ÖR ¢“°¢Ð§Ð