//! Tier-2 preset harnesses.
//!
//! Static data for well-known ACP harnesses that have bundled logos and
//! verified command/args. PATH-probed at discovery time (Detected badge);
//! not editable or deletable by users. Logos are bundled assets referenced
//! by id in the frontend `RUNTIME_LOGOS` map.
//!
//! Split out of `discovery.rs` so the preset table can grow without pushing
//! that module further over the file-size ratchet.

use std::path::PathBuf;

use super::normalize_agent_args;
use crate::managed_agents::{
    AcpAvailabilityStatus, AcpRuntimeCatalogEntry, AuthStatus, HarnessSource,
};

pub(crate) struct PresetHarness {
    pub id: &'static str,
    pub label: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub install_instructions_url: &'static str,
    pub install_hint: &'static str,
    /// Vendor CLI the ACP command wraps, when the preset is an adapter
    /// (e.g. Amp's `amp-acp` wraps the separately-installed `amp` CLI).
    /// Consulted only when the adapter is absent, so `AdapterMissing`
    /// replaces the misleading `NotInstalled` when the CLI is present but
    /// the adapter is not. Deliberately NOT fed through the builtins'
    /// full `classify_runtime` predicate: that would flip
    /// adapter-present/CLI-absent from today's `Available` to `CliMissing`
    /// (unselectable), and presets carry a single flat `install_hint`, so
    /// the `CliMissing` copy would tell the user to install the adapter
    /// they already have. `None` when the command IS the vendor CLI.
    pub underlying_cli: Option<&'static str>,
    /// How to sign this harness in to its model providers, surfaced once the
    /// command resolves. Presets run no auth probe (their CLIs expose no
    /// non-interactive login-status command), so this is static guidance
    /// rather than probe-derived state: it says how to authenticate, never
    /// that the user currently is or isn't. `None` when the harness needs no
    /// sign-in step or documents none we can quote verbatim.
    pub login_hint: Option<&'static str>,
}

/// Build the catalog entry for one preset harness through an injectable
/// resolver — the seam the preset loop consumes and tests bind.
///
/// Availability consumes only the adapter-missing arm of the builtin
/// predicate: adapter presence alone decides `Available` (exactly today's
/// behavior — an `amp-acp` without `amp` stays selectable), and
/// `underlying_cli` is consulted only when the adapter is absent, to
/// distinguish `AdapterMissing` (vendor CLI present) from `NotInstalled`
/// (neither found). See the `underlying_cli` field doc for why the full
/// `classify_runtime` predicate is deliberately not used here.
pub(crate) fn preset_catalog_entry(
    def: &PresetHarness,
    resolve: impl Fn(&str) -> Option<PathBuf>,
) -> AcpRuntimeCatalogEntry {
    let (availability, command, binary_path) = match resolve(def.command) {
        Some(path) => (
            AcpAvailabilityStatus::Available,
            Some(def.command.to_string()),
            Some(path.display().to_string()),
        ),
        None => {
            let underlying_cli_found = def
                .underlying_cli
                .map(|cli| resolve(cli).is_some())
                .unwrap_or(false);
            if underlying_cli_found {
                (AcpAvailabilityStatus::AdapterMissing, None, None)
            } else {
                (AcpAvailabilityStatus::NotInstalled, None, None)
            }
        }
    };
    let underlying_cli_path = def
        .underlying_cli
        .and_then(resolve)
        .map(|p| p.display().to_string());

    let default_args = normalize_agent_args(
        def.command,
        def.args.iter().map(|s| s.to_string()).collect(),
    );

    // Sign-in guidance is only actionable once the command exists; an absent
    // harness gets `install_hint` instead, and stacking both would tell the
    // user to run a binary they don't have.
    let login_hint = match availability {
        AcpAvailabilityStatus::Available => def.login_hint.map(str::to_string),
        _ => None,
    };

    AcpRuntimeCatalogEntry {
        id: def.id.to_string(),
        label: def.label.to_string(),
        // No remote URL — all preset icons are bundled assets.
        avatar_url: String::new(),
        availability,
        command,
        binary_path,
        default_args,
        mcp_command: None,
        model_env_var: None,
        provider_env_var: None,
        thinking_env_var: None,
        install_hint: def.install_hint.to_string(),
        install_instructions_url: def.install_instructions_url.to_string(),
        can_auto_install: false,
        // Kept false even for adapter presets: presets carry one flat
        // install_hint (the adapter's), so the requiresExternalCli
        // "CLI is missing" wording would pair the wrong noun with it.
        // The builtin path, with per-availability hints, is the only
        // consumer of the true case.
        requires_external_cli: false,
        underlying_cli_path,
        node_required: false,
        auth_status: AuthStatus::NotApplicable,
        login_hint,
        source: HarnessSource::Preset,
        // Preset entries have static, non-editable env; definition_env is empty.
        definition_env: Default::default(),
    }
}

pub(crate) const PRESET_HARNESSES: &[PresetHarness] = &[
    PresetHarness {
        id: "cursor",
        label: "Cursor",
        command: "cursor-agent",
        args: &["acp"],
        install_instructions_url: "https://cursor.com/downloads",
        install_hint: "Buzz talks to Cursor through the cursor-agent CLI's ACP mode.",
        underlying_cli: None,
        login_hint: None,
    },
    PresetHarness {
        id: "ompk",
        label: "Oh My PK",
        // The package installs three bins — `oh-my-pk`, `ompk`, and `omp`.
        // `omp` is deliberately NOT the probed command: upstream oh-my-pi
        // claims that name too, so probing it would light this row up for an
        // install that is not this harness. `ompk` is unique to the fork.
        command: "ompk",
        args: &["acp"],
        install_instructions_url: "https://github.com/kingkillery/oh-my-pk",
        install_hint: "Buzz talks to Oh My PK through its CLI's ACP mode (ompk acp).",
        underlying_cli: None,
        // Verified against the fork's `auth-broker` CLI: `login <provider>`
        // runs the OAuth dance in-process and writes the credential to the
        // local store, so it needs no broker deployment. Provider ids are the
        // fork's own (`anthropic`, `cursor`, `openai-codex`).
        login_hint: Some(
            "Sign in from a terminal with `ompk auth-broker login anthropic` \
             (also `cursor`, `openai-codex`), or run `ompk` and use `/login`.",
        ),
    },
    PresetHarness {
        id: "grok",
        label: "Grok Build",
        command: "grok",
        args: &["agent", "--always-approve", "stdio"],
        install_instructions_url: "https://build.x.ai/docs",
        install_hint: "Buzz talks to Grok Build through its CLI's agent stdio mode.",
        underlying_cli: None,
        login_hint: None,
    },
    PresetHarness {
        id: "opencode",
        label: "OpenCode",
        command: "opencode",
        args: &["acp"],
        install_instructions_url: "https://opencode.ai/docs",
        install_hint: "Buzz talks to OpenCode through its CLI's ACP mode (opencode acp).",
        underlying_cli: None,
        login_hint: None,
    },
    PresetHarness {
        id: "kimi",
        label: "Kimi Code",
        command: "kimi",
        args: &["acp"],
        install_instructions_url: "https://kimi.ai/download",
        install_hint: "Buzz talks to Kimi Code through its CLI's ACP mode (kimi acp).",
        underlying_cli: None,
        login_hint: None,
    },
    PresetHarness {
        id: "amp",
        label: "Amp",
        command: "amp-acp",
        args: &[],
        install_instructions_url: "https://github.com/tao12345666333/amp-acp",
        install_hint: "Buzz talks to the Amp CLI through the amp-acp adapter. Follow the setup guide to install the adapter so the amp-acp command is on your PATH.",
        underlying_cli: Some("amp"),
        login_hint: None,
    },
    PresetHarness {
        id: "hermes",
        label: "Hermes Agent",
        command: "hermes-acp",
        args: &[],
        install_instructions_url: "https://hermes-agent.nousresearch.com",
        install_hint: "Buzz talks to Hermes Agent through its hermes-acp command.",
        underlying_cli: None,
        login_hint: None,
    },
    PresetHarness {
        id: "openclaw",
        label: "OpenClaw",
        command: "openclaw",
        args: &["acp"],
        install_instructions_url: "https://docs.openclaw.ai/start/getting-started",
        install_hint: "Buzz talks to OpenClaw through its ACP mode (openclaw acp), which relies on the OpenClaw Gateway daemon. Follow the setup guide to install both.\n\n\
            ⚠️  Execution-locus note: `openclaw acp` runs tools inside the \
            OpenClaw Gateway daemon, not in the Desktop process. \
            Desktop-injected BUZZ_* env vars are visible to the `openclaw` \
            harness process itself, but do NOT automatically reach the \
            Gateway's execution environment. If your tools or agent logic \
            needs BUZZ_* credentials at execution time, set them on the \
            Gateway's own environment separately.",
        underlying_cli: None,
        login_hint: None,
    },
];

/// Return the static preset harness definitions as `HarnessDefinition` values.
///
/// Used by `warm_harness_registry_from_dir` to seed the loaded-harness registry
/// at startup before the frontend triggers a full discovery run.
pub(crate) fn preset_harness_definitions(
) -> Vec<crate::managed_agents::custom_harnesses::HarnessDefinition> {
    PRESET_HARNESSES
        .iter()
        .map(
            |p| crate::managed_agents::custom_harnesses::HarnessDefinition {
                id: p.id.to_string(),
                label: p.label.to_string(),
                command: p.command.to_string(),
                args: p.args.iter().map(|s| s.to_string()).collect(),
                env: std::collections::BTreeMap::new(),
                install_instructions_url: p.install_instructions_url.to_string(),
                install_hint: p.install_hint.to_string(),
            },
        )
        .collect()
}

/// Return the static slice of preset harness IDs.
///
/// Used by `check_id_collision` in `custom_harnesses` to derive the reserved-ID
/// set from the single source of truth (`PRESET_HARNESSES`) rather than a
/// hand-maintained copy.  Adding a preset automatically reserves its ID.
pub(crate) fn preset_harness_ids() -> &'static [&'static str] {
    // `PRESET_HARNESSES` is `'static`; we project its `id` fields.
    // Computed once via OnceLock to avoid repeated allocations on hot paths.
    use std::sync::OnceLock;
    static IDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    IDS.get_or_init(|| PRESET_HARNESSES.iter().map(|p| p.id).collect())
        .as_slice()
}
