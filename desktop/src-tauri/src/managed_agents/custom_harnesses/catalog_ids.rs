/// IDs reserved for the compiled-in catalog. A custom definition whose `id`
/// collides with a built-in or preset is rejected to prevent shadowing (e.g. a
/// file called `cursor.json` hiding the pre-existing tier-2 preset).
///
/// Derived from the authoritative rich-runtime and flat-preset tables — no
/// hand-maintained copy. Adding or promoting a runtime automatically reserves
/// its ID without a second collision-list edit.
fn builtin_ids() -> impl Iterator<Item = &'static str> {
    crate::managed_agents::discovery::known_runtime_ids().chain(
        crate::managed_agents::discovery::preset_harness_ids()
            .iter()
            .copied(),
    )
}

/// Return an error string if `id` conflicts with a built-in harness ID.
pub(crate) fn check_id_collision(id: &str) -> Result<(), String> {
    if builtin_ids().any(|reserved| reserved.eq_ignore_ascii_case(id)) {
        return Err(format!(
            "id {:?} is reserved for a built-in harness and cannot be overridden",
            id
        ));
    }
    Ok(())
}
