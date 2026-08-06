//! Tier-2 preset harness catalog entries (`discovery::presets`).

use std::path::PathBuf;

use super::super::presets::{preset_catalog_entry, PresetHarness, PRESET_HARNESSES};
use crate::managed_agents::AcpAvailabilityStatus;

/// Amp-shaped preset: an ACP adapter (`amp-acp`) wrapping a separately
/// installed vendor CLI (`amp`).
const ADAPTER_PRESET: PresetHarness = PresetHarness {
    id: "amp-test",
    label: "Amp Test",
    command: "amp-acp",
    args: &[],
    install_instructions_url: "https://example.com/install",
    install_hint: "Install the amp-acp npm adapter.",
    underlying_cli: Some("amp"),
    login_hint: Some("Run `amp-test login`."),
};

#[test]
fn preset_entry_adapter_missing_when_underlying_cli_present() {
    // Vendor CLI resolves, adapter does not — the state Tyler's Amp
    // hand-test hit. Must NOT degrade to the misleading NotInstalled.
    let entry = preset_catalog_entry(&ADAPTER_PRESET, |cmd| {
        (cmd == "amp").then(|| PathBuf::from("/usr/local/bin/amp"))
    });
    assert_eq!(entry.availability, AcpAvailabilityStatus::AdapterMissing);
    assert!(entry.command.is_none());
    assert!(entry.binary_path.is_none());
    assert_eq!(
        entry.underlying_cli_path.as_deref(),
        Some("/usr/local/bin/amp")
    );
    assert!(!entry.requires_external_cli);
    assert_eq!(entry.install_hint, "Install the amp-acp npm adapter.");
}

#[test]
fn preset_entry_not_installed_when_both_missing() {
    let entry = preset_catalog_entry(&ADAPTER_PRESET, |_| None);
    assert_eq!(entry.availability, AcpAvailabilityStatus::NotInstalled);
    assert!(entry.underlying_cli_path.is_none());
    assert!(!entry.requires_external_cli);
}

#[test]
fn preset_entry_available_when_adapter_and_cli_present() {
    let entry = preset_catalog_entry(&ADAPTER_PRESET, |cmd| match cmd {
        "amp-acp" => Some(PathBuf::from("/usr/local/bin/amp-acp")),
        "amp" => Some(PathBuf::from("/usr/local/bin/amp")),
        _ => None,
    });
    assert_eq!(entry.availability, AcpAvailabilityStatus::Available);
    assert_eq!(entry.command.as_deref(), Some("amp-acp"));
    assert_eq!(entry.binary_path.as_deref(), Some("/usr/local/bin/amp-acp"));
    assert_eq!(
        entry.underlying_cli_path.as_deref(),
        Some("/usr/local/bin/amp")
    );
}

#[test]
fn preset_entry_stays_available_when_adapter_present_but_cli_absent() {
    // Wren's regression guard: today an `amp-acp` install without `amp`
    // is Available and selectable. Feeding underlying_cli through the
    // FULL classify_runtime predicate would flip this to CliMissing
    // (unselectable, with backwards install copy) — the adapter-missing
    // arm is the only one presets consume.
    let entry = preset_catalog_entry(&ADAPTER_PRESET, |cmd| {
        (cmd == "amp-acp").then(|| PathBuf::from("/usr/local/bin/amp-acp"))
    });
    assert_eq!(entry.availability, AcpAvailabilityStatus::Available);
    assert_eq!(entry.command.as_deref(), Some("amp-acp"));
    assert_eq!(entry.binary_path.as_deref(), Some("/usr/local/bin/amp-acp"));
    assert!(entry.underlying_cli_path.is_none());
}

#[test]
fn preset_entry_without_underlying_cli_stays_simple() {
    // Most presets: the command IS the vendor CLI. No external-CLI flag,
    // absent command means plain NotInstalled.
    let preset = PresetHarness {
        underlying_cli: None,
        ..ADAPTER_PRESET
    };
    let entry = preset_catalog_entry(&preset, |_| None);
    assert_eq!(entry.availability, AcpAvailabilityStatus::NotInstalled);
    assert!(!entry.requires_external_cli);
    assert!(entry.underlying_cli_path.is_none());
}

#[test]
fn preset_entry_carries_login_hint_only_once_the_command_resolves() {
    // Sign-in copy is actionable only when the binary exists; a missing
    // harness must show install copy alone, never "run `<cmd> login`" for a
    // command the user doesn't have.
    let installed = preset_catalog_entry(&ADAPTER_PRESET, |cmd| match cmd {
        "amp-acp" => Some(PathBuf::from("/usr/local/bin/amp-acp")),
        "amp" => Some(PathBuf::from("/usr/local/bin/amp")),
        _ => None,
    });
    assert_eq!(
        installed.login_hint.as_deref(),
        Some("Run `amp-test login`.")
    );

    let absent = preset_catalog_entry(&ADAPTER_PRESET, |_| None);
    assert_eq!(absent.availability, AcpAvailabilityStatus::NotInstalled);
    assert!(absent.login_hint.is_none());
}

#[test]
fn preset_entry_without_login_hint_stays_silent() {
    let preset = PresetHarness {
        login_hint: None,
        ..ADAPTER_PRESET
    };
    let entry = preset_catalog_entry(&preset, |cmd| {
        (cmd == "amp-acp").then(|| PathBuf::from("/usr/local/bin/amp-acp"))
    });
    assert_eq!(entry.availability, AcpAvailabilityStatus::Available);
    assert!(entry.login_hint.is_none());
}

/// The Oh My PK preset must probe `ompk`, not `omp`: the two ACP harnesses
/// coexist, and upstream oh-my-pi owns `omp`, so probing it would report the
/// fork-specific harness as installed for an unrelated CLI.
#[test]
fn ompk_preset_probes_the_fork_specific_binary_and_documents_sign_in() {
    let preset = PRESET_HARNESSES
        .iter()
        .find(|p| p.id == "ompk")
        .expect("ompk preset present");
    assert_eq!(preset.command, "ompk");
    assert_eq!(preset.args, &["acp"]);

    let hint = preset.login_hint.expect("ompk documents its sign-in path");
    for provider in ["anthropic", "cursor", "openai-codex"] {
        assert!(
            hint.contains(provider),
            "sign-in hint must name the {provider} provider id"
        );
    }

    let upstream_omp = PRESET_HARNESSES
        .iter()
        .find(|p| p.id == "omp")
        .expect("upstream omp preset remains available alongside ompk");
    assert_eq!(upstream_omp.command, "omp");
    assert_eq!(upstream_omp.args, &["acp"]);
}
