//! Provider-specific readiness rules for the native Pkzz agent.
//!
//! Split out of `readiness.rs` so the per-provider table has one home: each
//! provider contributes a model env key and, usually, a credential env key.
//! Mirrors `agentConfigOptions.tsx`'s `PROVIDER_CREDENTIAL_CONFIG` on the
//! frontend and the resolution order in `buzz-agent/src/config.rs` — keep the
//! three in sync.

/// Provider-specific fallback env key holding the model id.
///
/// `BUZZ_AGENT_MODEL` takes precedence everywhere; this is the per-provider
/// key consulted when it is absent.
pub(super) fn provider_model_env_key(provider: Option<&str>) -> Option<&'static str> {
    match provider {
        Some("databricks") | Some("databricks_v2") | Some("databricks-v2") => {
            Some("DATABRICKS_MODEL")
        }
        Some("anthropic") => Some("ANTHROPIC_MODEL"),
        Some("openai") | Some("openai-compat") => Some("OPENAI_COMPAT_MODEL"),
        // Cline rides the OpenAI-compatible route but carries its own model
        // and credential keys (buzz-agent/src/config.rs).
        Some("cline") => Some("CLINE_MODEL"),
        Some("openrouter") => Some("OPENROUTER_MODEL"),
        _ => None,
    }
}

/// The credential env key a provider still needs, if any.
///
/// `env_key_missing` treats a key present with an empty value as absent,
/// matching the dialog's `(envVars[key] ?? "").length === 0` check. Returns
/// `None` for unknown providers: the caller already reports the missing
/// provider itself as a normalized-field gap.
pub(super) fn missing_provider_credential(
    provider: Option<&str>,
    env_key_missing: impl Fn(&str) -> bool,
) -> Option<&'static str> {
    match provider {
        Some("anthropic") if env_key_missing("ANTHROPIC_API_KEY") => Some("ANTHROPIC_API_KEY"),
        Some("openai") | Some("openai-compat") if env_key_missing("OPENAI_COMPAT_API_KEY") => {
            Some("OPENAI_COMPAT_API_KEY")
        }
        // DATABRICKS_HOST is hard-required; DATABRICKS_TOKEN is optional
        // (OAuth PKCE is the normal path — see buzz-agent/src/config.rs:143).
        Some("databricks") | Some("databricks_v2") | Some("databricks-v2")
            if env_key_missing("DATABRICKS_HOST") =>
        {
            Some("DATABRICKS_HOST")
        }
        Some("openrouter") if env_key_missing("OPENROUTER_API_KEY") => Some("OPENROUTER_API_KEY"),
        // buzz-agent accepts either key for Cline, so only flag the gap when
        // neither is present.
        Some("cline")
            if env_key_missing("CLINE_API_KEY") && env_key_missing("OPENAI_COMPAT_API_KEY") =>
        {
            Some("CLINE_API_KEY")
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_missing(_key: &str) -> bool {
        true
    }

    #[test]
    fn cline_uses_its_own_model_and_credential_keys() {
        assert_eq!(provider_model_env_key(Some("cline")), Some("CLINE_MODEL"));
        assert_eq!(
            missing_provider_credential(Some("cline"), all_missing),
            Some("CLINE_API_KEY")
        );
    }

    #[test]
    fn cline_accepts_the_openai_compatible_key_as_a_fallback() {
        // buzz-agent reads CLINE_API_KEY, then OPENAI_COMPAT_API_KEY; a user
        // who set only the latter is configured, not missing a credential.
        let only_compat = |key: &str| key != "OPENAI_COMPAT_API_KEY";
        assert_eq!(
            missing_provider_credential(Some("cline"), only_compat),
            None
        );
    }

    #[test]
    fn known_providers_keep_their_credentials() {
        for (provider, key) in [
            ("anthropic", "ANTHROPIC_API_KEY"),
            ("openai", "OPENAI_COMPAT_API_KEY"),
            ("openrouter", "OPENROUTER_API_KEY"),
            ("databricks", "DATABRICKS_HOST"),
        ] {
            assert_eq!(
                missing_provider_credential(Some(provider), all_missing),
                Some(key),
                "{provider} must still report its credential"
            );
        }
    }

    #[test]
    fn unknown_or_absent_provider_reports_no_credential_gap() {
        assert_eq!(missing_provider_credential(None, all_missing), None);
        assert_eq!(
            missing_provider_credential(Some("not-a-provider"), all_missing),
            None
        );
        assert_eq!(provider_model_env_key(None), None);
    }
}
