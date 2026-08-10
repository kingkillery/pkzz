use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::managed_agents::custom_harnesses::{
    loaded_harness_registry, lookup_loaded_harness_by_id, HarnessDefinition,
};

use super::presets::{preset_harness_by_id, PRESET_HARNESSES};
use super::{
    known_acp_runtime_exact, normalize_command_identity, resolve_command, KnownAcpRuntime,
    DANGLING_HARNESS_PREFIX, KNOWN_ACP_RUNTIMES,
};

/// One backend-owned catalog identity and its complete launch defaults.
///
/// Consumers must treat `command`, `default_args`, and `definition_env()` as an
/// atomic result. A renderer-supplied command or copied defaults must never
/// override an ID-backed spec implicitly.
#[derive(Debug, Clone)]
pub(crate) struct CatalogHarnessLaunchSpec {
    pub runtime_id: String,
    pub command: String,
    pub default_args: Vec<String>,
    pub known_runtime: Option<&'static KnownAcpRuntime>,
    pub definition: Option<Arc<HarnessDefinition>>,
}

impl CatalogHarnessLaunchSpec {
    /// Definition environment belonging to the same identity as the command
    /// and default argv. Rich runtimes keep their env in metadata; flat
    /// preset/custom runtimes keep it on the loaded definition.
    pub(crate) fn definition_env(&self) -> BTreeMap<String, String> {
        if let Some(runtime) = self.known_runtime {
            return runtime
                .default_env
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect();
        }

        self.definition
            .as_ref()
            .map(|definition| definition.env.clone())
            .unwrap_or_default()
    }
}

fn dangling_harness_id(id: &str) -> String {
    format!("{DANGLING_HARNESS_PREFIX}{id}")
}

fn rich_launch_spec(
    runtime: &'static KnownAcpRuntime,
    mut command_is_available: impl FnMut(&str) -> bool,
) -> CatalogHarnessLaunchSpec {
    let command = runtime
        .commands
        .iter()
        .copied()
        .find(|command| command_is_available(command))
        .or_else(|| runtime.commands.first().copied())
        .unwrap_or(runtime.id)
        .to_string();

    CatalogHarnessLaunchSpec {
        runtime_id: runtime.id.to_string(),
        command,
        default_args: runtime
            .default_args
            .iter()
            .map(|arg| (*arg).to_string())
            .collect(),
        known_runtime: Some(runtime),
        definition: None,
    }
}

fn flat_launch_spec(definition: HarnessDefinition) -> CatalogHarnessLaunchSpec {
    CatalogHarnessLaunchSpec {
        runtime_id: definition.id.clone(),
        command: definition.command.clone(),
        default_args: definition.args.clone(),
        known_runtime: None,
        definition: Some(Arc::new(definition)),
    }
}

fn resolve_catalog_harness_by_id_with(
    id: &str,
    command_is_available: impl FnMut(&str) -> bool,
) -> Result<CatalogHarnessLaunchSpec, String> {
    if let Some(runtime) = known_acp_runtime_exact(id) {
        return Ok(rich_launch_spec(runtime, command_is_available));
    }

    if let Some(preset) = preset_harness_by_id(id) {
        return Ok(flat_launch_spec(HarnessDefinition {
            id: preset.id.to_string(),
            label: preset.label.to_string(),
            command: preset.command.to_string(),
            args: preset.args.iter().map(|arg| (*arg).to_string()).collect(),
            env: BTreeMap::new(),
            install_instructions_url: preset.install_instructions_url.to_string(),
            install_hint: preset.install_hint.to_string(),
        }));
    }

    if let Some(definition) = lookup_loaded_harness_by_id(id) {
        return Ok(CatalogHarnessLaunchSpec {
            runtime_id: definition.id.clone(),
            command: definition.command.clone(),
            default_args: definition.args.clone(),
            known_runtime: None,
            definition: Some(definition),
        });
    }

    Err(dangling_harness_id(id))
}

/// Resolve one exact catalog ID across rich metadata, flat presets, and the
/// warmed custom registry. Unknown/deleted IDs fail explicitly and never fall
/// through to the bundled legacy command.
pub(crate) fn resolve_catalog_harness_by_id(id: &str) -> Result<CatalogHarnessLaunchSpec, String> {
    resolve_catalog_harness_by_id_with(id, |command| resolve_command(command).is_some())
}

/// Recover a catalog identity from a legacy explicit command only when the
/// normalized command/id/alias has exactly one matching runtime.
///
/// This helper is intentionally unsuitable for new requests: command strings
/// can collide, and `ManagedAgentRecord.agent_command` is only a stale
/// create-time snapshot. New records persist `launch_runtime_id` directly.
pub(crate) fn unique_catalog_runtime_id_for_command(command: &str) -> Option<String> {
    let normalized = normalize_command_identity(command);
    if normalized.is_empty() {
        return None;
    }

    let mut matches = BTreeSet::new();

    for runtime in KNOWN_ACP_RUNTIMES {
        if normalized == runtime.id
            || runtime
                .commands
                .iter()
                .any(|candidate| normalize_command_identity(candidate) == normalized)
            || runtime
                .aliases
                .iter()
                .any(|alias| normalize_command_identity(alias) == normalized)
        {
            matches.insert(runtime.id.to_string());
        }
    }

    for preset in PRESET_HARNESSES {
        if normalized == preset.id || normalize_command_identity(preset.command) == normalized {
            matches.insert(preset.id.to_string());
        }
    }

    let registry = loaded_harness_registry()
        .read()
        .unwrap_or_else(|error| error.into_inner());
    for definition in registry.iter() {
        if normalized == definition.id
            || normalize_command_identity(&definition.command) == normalized
        {
            matches.insert(definition.id.clone());
        }
    }

    let mut matches = matches.into_iter();
    let runtime_id = matches.next()?;
    matches.next().is_none().then_some(runtime_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_agents::custom_harnesses::{
        registry_test_lock, update_loaded_harness_registry, warm_harness_registry_from_dir,
    };

    fn custom_definition(id: &str, command: &str, args: &[&str]) -> HarnessDefinition {
        HarnessDefinition {
            id: id.to_string(),
            label: id.to_string(),
            command: command.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            env: BTreeMap::from([("CUSTOM_TOKEN".to_string(), "value".to_string())]),
            install_instructions_url: String::new(),
            install_hint: String::new(),
        }
    }

    #[test]
    fn ompk_resolves_to_exact_acp_launch_spec() {
        let spec = resolve_catalog_harness_by_id_with("ompk", |_| false).unwrap();
        assert_eq!(spec.runtime_id, "ompk");
        assert_eq!(spec.command, "ompk");
        assert_eq!(spec.default_args, vec!["acp"]);
        assert!(spec.definition_env().is_empty());
    }

    #[test]
    fn rich_runtime_uses_installed_alternate_command_without_losing_id() {
        let spec =
            resolve_catalog_harness_by_id_with("claude", |command| command == "claude-code-acp")
                .unwrap();
        assert_eq!(spec.runtime_id, "claude");
        assert_eq!(spec.command, "claude-code-acp");
    }

    #[test]
    fn warmed_custom_definition_edits_are_observed_by_id() {
        let _guard = registry_test_lock();
        update_loaded_harness_registry(vec![custom_definition(
            "custom-live",
            "first-command",
            &["first"],
        )]);
        let first = resolve_catalog_harness_by_id_with("custom-live", |_| false).unwrap();
        assert_eq!(first.command, "first-command");
        assert_eq!(first.default_args, vec!["first"]);

        update_loaded_harness_registry(vec![custom_definition(
            "custom-live",
            "second-command",
            &["second"],
        )]);
        let second = resolve_catalog_harness_by_id_with("custom-live", |_| false).unwrap();
        assert_eq!(second.command, "second-command");
        assert_eq!(second.default_args, vec!["second"]);
        assert_eq!(
            second
                .definition_env()
                .get("CUSTOM_TOKEN")
                .map(String::as_str),
            Some("value")
        );

        warm_harness_registry_from_dir(None);
    }

    #[test]
    fn deleted_custom_id_is_typed_dangling_error() {
        let _guard = registry_test_lock();
        update_loaded_harness_registry(Vec::new());
        let error = resolve_catalog_harness_by_id_with("deleted-custom", |_| false).unwrap_err();
        assert_eq!(error, "DANGLING_HARNESS_ID:deleted-custom");
        warm_harness_registry_from_dir(None);
    }

    #[test]
    fn legacy_command_inference_requires_a_unique_identity() {
        let _guard = registry_test_lock();
        update_loaded_harness_registry(Vec::new());
        assert_eq!(
            unique_catalog_runtime_id_for_command("C:/tools/ompk.cmd").as_deref(),
            Some("ompk")
        );

        update_loaded_harness_registry(vec![custom_definition("custom-ompk-command", "ompk", &[])]);
        assert_eq!(unique_catalog_runtime_id_for_command("ompk"), None);
        warm_harness_registry_from_dir(None);
    }
}
