/// Returns the adapter install commands that `install_acp_runtime_blocking` would
/// run for `runtime_id` given a resolved adapter binary at `adapter_path` (or `None` if not found).
/// Returns `None` when no install is needed; `Some(cmds)` when adapter is missing or outdated.
///
/// For the codex **outdated** case, returns a two-step reinstall: uninstall `@zed-industries/codex-acp`
/// then install `@agentclientprotocol/codex-acp` (npm ≥7 refuses to overwrite a bin from another pkg).
/// For the **missing** case, catalog's `adapter_install_commands` are used as-is.
/// Pure planning function: never spawns a process. Tests use it to assert commands without real npm.
pub(crate) fn plan_adapter_install<'c>(
    runtime_id: &str,
    adapter_path: Option<&std::path::Path>,
    adapter_install_commands: &'c [&'c str],
    adapter_probe_path: Option<&str>,
) -> Option<Vec<&'c str>> {
    match adapter_path {
        // Adapter present and current — no install needed.
        Some(_) if runtime_id != "codex" => None,
        Some(path)
            if !crate::managed_agents::codex_adapter_is_outdated_with_path(
                path,
                adapter_probe_path,
            ) =>
        {
            None
        }
        // Codex adapter is outdated: uninstall the old package first so npm
        // doesn't hit EEXIST on the shared `codex-acp` bin-link, then install.
        Some(_) => Some(vec![
            "npm uninstall -g @zed-industries/codex-acp",
            "npm install -g @agentclientprotocol/codex-acp",
        ]),
        // Adapter missing: use the catalog's install commands directly.
        None => Some(adapter_install_commands.to_vec()),
    }
}
