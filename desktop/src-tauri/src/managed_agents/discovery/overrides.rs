//! The `agent_command_override` decision family: divergence comparison,
//! create-time and update-time resolution, and the record apply. Child module
//! of `discovery` (split under the desktop file-size cap) so the override
//! decisions stay next to the resolution ladder they feed.

use super::{effective_agent_command, known_acp_runtime};

/// Decide whether a user-picked harness command is an explicit per-instance
/// pin or merely the persona's own runtime restated. Returns the override to
/// persist: `Some(picked)` when it diverges from the persona, `None` when it
/// inherits.
///
/// Comparison is by RUNTIME IDENTITY, not raw string: a persona on the `claude`
/// runtime resolves to `claude-agent-acp`, but a client with only the
/// `claude-code-acp` adapter installed sends that command instead. Both map to
/// the same `claude` runtime, so neither is a real divergence — string equality
/// would wrongly bake a pin. An unknown/custom command (no matching runtime)
/// only inherits when it exactly equals the persona command.
pub fn divergent_agent_command_override(
    persona_id: Option<&str>,
    personas: &[crate::managed_agents::types::AgentDefinition],
    picked_command: Option<&str>,
) -> Option<String> {
    let picked = picked_command
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let persona_command = effective_agent_command(persona_id, personas, None);
    let same_runtime = match (
        known_acp_runtime(picked),
        known_acp_runtime(&persona_command),
    ) {
        (Some(a), Some(b)) => std::ptr::eq(a, b),
        _ => picked == persona_command,
    };
    if same_runtime {
        None
    } else {
        Some(picked.to_string())
    }
}

/// Decide the `agent_command_override` to persist at AGENT UPDATE time.
///
/// The edit dialog sends `agent_command` as a tri-state string: the empty
/// "inherit from persona" sentinel (clear the pin), or a concrete command
/// (pin). Resolution:
///
/// - EMPTY / whitespace → the inherit sentinel: always `None` regardless of
///   `harness_override`, so toggling "Inherit runtime from persona" clears the
///   pin.
/// - DELIBERATE OVERRIDE (`harness_override` true, persona linked): the user
///   explicitly picked a runtime/Custom command in the dialog. This is a real
///   pin and is preserved VERBATIM — even when the picked command maps to, or
///   is byte-identical to, the persona's own runtime command. Selecting "Custom
///   command" and saving e.g. `goose` for a goose persona is a deliberate act
///   to freeze the harness against future persona runtime edits; dropping it
///   back to inherit (as [`divergent_agent_command_override`] would) defeats
///   that intent. Unlike the create-time path, there is no byte-identical
///   exception here: at create the command is machine-derived from the persona,
///   so equality means "no user divergence"; at update an equal command reached
///   the force branch only because the user picked Custom, which IS the
///   divergence.
/// - NO OVERRIDE INTENT (`harness_override` false) or NO PERSONA: defer to
///   [`divergent_agent_command_override`], which keeps the persona authoritative
///   and treats a same-runtime restatement as inherit.
pub fn update_time_agent_command_override(
    persona_id: Option<&str>,
    personas: &[crate::managed_agents::types::AgentDefinition],
    picked_command: Option<&str>,
    harness_override: bool,
) -> Option<String> {
    let picked = picked_command
        .map(str::trim)
        .filter(|value| !value.is_empty())?;

    if persona_id.is_some() && harness_override {
        return Some(picked.to_string());
    }

    divergent_agent_command_override(persona_id, personas, Some(picked))
}

/// Apply an explicit `agent_command` edit to `record`: persist the override
/// pin decided by [`update_time_agent_command_override`], and on the inherit
/// sentinel (empty/whitespace command) also clear the materialized
/// `record.runtime` so the resolution ladder falls through to the live
/// definition immediately instead of silently keeping the stale instance copy.
///
/// The runtime clear is guarded on a live persona link: for a definition-less
/// record the materialized runtime is the only harness source left after the
/// override clear, so a stray empty `agent_command` from a non-dialog caller
/// must not change what the agent runs.
pub fn apply_agent_command_update(
    record: &mut crate::managed_agents::types::ManagedAgentRecord,
    personas: &[crate::managed_agents::types::AgentDefinition],
    agent_command: &str,
    harness_override: bool,
) {
    record.agent_command_override = update_time_agent_command_override(
        record.persona_id.as_deref(),
        personas,
        Some(agent_command),
        harness_override,
    );
    if agent_command.trim().is_empty() && record.persona_id.is_some() {
        record.runtime = None;
    }
}

/// Decide the `agent_command_override` to persist at AGENT CREATE time.
///
/// A persona-backed create receives its harness command from
/// `resolvePersonaRuntime` (frontend), which produces a divergent command in two
/// distinct cases that the backend MUST tell apart:
///
/// - DELIBERATE OVERRIDE (`harness_override` true): the user explicitly picked a
///   runtime command in UI that exposes a runtime selector. This is a real pin
///   and is preserved when it differs from the command inheritance would spawn,
///   including installed aliases such as `claude-code-acp`.
/// - MISSING-RUNTIME FALLBACK (`harness_override` false): the persona's runtime
///   isn't installed locally, so `resolvePersonaRuntime` substitutes a fallback
///   default. This is NOT a pin — baking it would freeze the agent on the fallback
///   harness even after the persona's runtime is installed and the persona is
///   re-edited, the exact bug this resolver chain exists to prevent. Stores `None`
///   so the persona stays authoritative.
///
/// `isOverridden` from `resolvePersonaRuntime` cannot distinguish these — it is
/// `true` for BOTH — so the caller must thread the explicit user-intent bit.
///
/// Persona-less creates (`persona_id` is `None`, e.g. the standalone
/// CreateAgentDialog) have no persona to inherit, so the picked command is always a
/// real pin and is preserved via `divergent_agent_command_override` regardless of
/// `harness_override`.
pub fn create_time_agent_command_override(
    persona_id: Option<&str>,
    personas: &[crate::managed_agents::types::AgentDefinition],
    picked_command: Option<&str>,
    harness_override: bool,
) -> Option<String> {
    if persona_id.is_some() && !harness_override {
        return None;
    }

    if persona_id.is_some() && harness_override {
        let picked = picked_command
            .map(str::trim)
            .filter(|value| !value.is_empty())?;
        let inherited_command = effective_agent_command(persona_id, personas, None);
        return (picked != inherited_command).then(|| picked.to_string());
    }

    divergent_agent_command_override(persona_id, personas, picked_command)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreateHarnessSelection {
    pub launch_runtime_id: Option<String>,
    pub raw_command_explicit: bool,
    pub command: String,
    pub command_override: Option<String>,
    pub args: Vec<String>,
}

pub(crate) fn resolve_create_harness_selection(
    persona_id: Option<&str>,
    personas: &[crate::managed_agents::types::AgentDefinition],
    runtime_id: Option<Option<&str>>,
    picked_command: Option<&str>,
    args: &[String],
    harness_override: bool,
) -> Result<CreateHarnessSelection, String> {
    let explicit_args = args
        .iter()
        .map(|arg| arg.trim().to_string())
        .filter(|arg| !arg.is_empty())
        .collect::<Vec<_>>();

    if let Some(Some(runtime_id)) = runtime_id {
        let runtime_id = runtime_id.trim();
        if runtime_id.is_empty() {
            return Err("runtimeId must not be empty".to_string());
        }
        let spec = super::resolve_catalog_harness_by_id(runtime_id)?;
        let command_override =
            (persona_id.is_none() || harness_override).then(|| spec.command.clone());
        return Ok(CreateHarnessSelection {
            launch_runtime_id: Some(spec.runtime_id),
            raw_command_explicit: false,
            command: spec.command,
            command_override,
            args: explicit_args,
        });
    }

    if matches!(runtime_id, Some(None)) {
        let command = picked_command
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .ok_or_else(|| {
                "runtimeId null requires a non-empty raw custom agentCommand".to_string()
            })?;
        return Ok(CreateHarnessSelection {
            launch_runtime_id: None,
            raw_command_explicit: true,
            command: command.to_string(),
            command_override: Some(command.to_string()),
            args: explicit_args,
        });
    }

    let command_override =
        create_time_agent_command_override(persona_id, personas, picked_command, harness_override);
    let command = effective_agent_command(persona_id, personas, command_override.as_deref());
    Ok(CreateHarnessSelection {
        launch_runtime_id: None,
        raw_command_explicit: false,
        args: super::normalize_agent_args(&command, explicit_args),
        command,
        command_override,
    })
}

pub(crate) fn apply_harness_update(
    record: &mut crate::managed_agents::types::ManagedAgentRecord,
    personas: &[crate::managed_agents::types::AgentDefinition],
    runtime_id: Option<Option<&str>>,
    agent_command: Option<&str>,
    harness_override: bool,
) -> Result<(), String> {
    match runtime_id {
        Some(Some(runtime_id)) => {
            let runtime_id = runtime_id.trim();
            if runtime_id.is_empty() {
                return Err("runtimeId must not be empty".to_string());
            }
            let spec = super::resolve_catalog_harness_by_id(runtime_id)?;
            record.launch_runtime_id = Some(spec.runtime_id);
            record.raw_command_explicit = false;
            record.agent_command = spec.command.clone();
            record.agent_command_override =
                (record.persona_id.is_none() || harness_override).then_some(spec.command);
        }
        Some(None) => {
            let picked = agent_command
                .map(str::trim)
                .filter(|command| !command.is_empty())
                .ok_or_else(|| {
                    "runtimeId null requires a non-empty raw custom agentCommand".to_string()
                })?;
            record.launch_runtime_id = None;
            record.raw_command_explicit = true;
            record.agent_command = picked.to_string();
            record.agent_command_override = update_time_agent_command_override(
                record.persona_id.as_deref(),
                personas,
                Some(picked),
                harness_override,
            )
            .or_else(|| Some(picked.to_string()));
        }
        None => {
            if let Some(agent_command) = agent_command {
                apply_agent_command_update(record, personas, agent_command, harness_override);
            }
        }
    }
    Ok(())
}
