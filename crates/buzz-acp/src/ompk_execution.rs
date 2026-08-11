//! Structured PKZZ -> OMPK execution requests carried by signed chat events.
//!
//! The integration deliberately stops at OMPK's existing ACP boundary. PKZZ
//! selects the requested session working directory; OMPK owns the agent,
//! worker, and tool execution inside that session.

use nostr::Event;
use std::path::{Path, PathBuf};

pub const EXECUTION_TAG: &str = "ompk-execution";
pub const CWD_TAG: &str = "ompk-cwd";
const CONTRACT_VERSION: &str = "1";
const UNSUPPORTED_PLACEMENT_TAGS: &[&str] = &[
    "ompk-agent",
    "ompk-host",
    "ompk-machine",
    "ompk-mode",
    "ompk-project",
    "ompk-repo",
    "ompk-runner",
    "ompk-workspace",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmpkExecutionRequest {
    /// Stable correlation identity: the signed PKZZ event ID.
    pub execution_id: String,
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct OmpkExecutionPolicy {
    pub enabled: bool,
    pub allowed_workspaces: Vec<PathBuf>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OmpkExecutionError {
    #[error("OMPK execution requests are disabled")]
    Disabled,
    #[error("invalid OMPK execution request")]
    InvalidContract,
    #[error("OMPK cwd placement requires the OMPK runtime")]
    UnsupportedRuntime,
    #[error("requested OMPK placement field is not supported by contract version 1")]
    UnsupportedPlacement,
    #[error("requested OMPK cwd must be an absolute directory")]
    InvalidCwd,
    #[error("requested OMPK cwd is outside the configured allowed workspaces")]
    CwdNotAllowed,
    #[error("configured OMPK allowed workspace must be an existing absolute directory")]
    InvalidAllowedWorkspace,
}

impl OmpkExecutionPolicy {
    /// Build a policy from operator-controlled workspace roots.
    ///
    /// Roots are canonicalized once at startup. Rejecting an invalid root is
    /// intentional: silently dropping a misspelled allowlist entry would turn
    /// a placement request into an opaque runtime failure later.
    pub fn new(
        enabled: bool,
        allowed_workspaces: Vec<PathBuf>,
    ) -> Result<Self, OmpkExecutionError> {
        let mut canonical_roots = Vec::with_capacity(allowed_workspaces.len());
        for root in allowed_workspaces {
            if !root.is_absolute() {
                return Err(OmpkExecutionError::InvalidAllowedWorkspace);
            }
            let canonical = root
                .canonicalize()
                .map_err(|_| OmpkExecutionError::InvalidAllowedWorkspace)?;
            let canonical = normalize_windows_verbatim_path(canonical);
            if !canonical.is_dir() {
                return Err(OmpkExecutionError::InvalidAllowedWorkspace);
            }
            if !canonical_roots.contains(&canonical) {
                canonical_roots.push(canonical);
            }
        }
        Ok(Self {
            enabled,
            allowed_workspaces: canonical_roots,
        })
    }

    pub fn parse_event(
        &self,
        event: &Event,
        harness_name: &str,
    ) -> Result<Option<OmpkExecutionRequest>, OmpkExecutionError> {
        let execution_tags: Vec<&[String]> = event
            .tags
            .iter()
            .map(|tag| tag.as_slice())
            .filter(|tag| tag.first().map(String::as_str) == Some(EXECUTION_TAG))
            .collect();
        let cwd_tags: Vec<&[String]> = event
            .tags
            .iter()
            .map(|tag| tag.as_slice())
            .filter(|tag| tag.first().map(String::as_str) == Some(CWD_TAG))
            .collect();
        let has_unsupported_placement = event.tags.iter().any(|tag| {
            tag.as_slice()
                .first()
                .map(String::as_str)
                .is_some_and(|name| UNSUPPORTED_PLACEMENT_TAGS.contains(&name))
        });

        if execution_tags.is_empty() && cwd_tags.is_empty() && !has_unsupported_placement {
            return Ok(None);
        }
        if !self.enabled {
            return Err(OmpkExecutionError::Disabled);
        }
        if has_unsupported_placement {
            return Err(OmpkExecutionError::UnsupportedPlacement);
        }
        if execution_tags.len() != 1
            || execution_tags[0].len() != 2
            || execution_tags[0].get(1).map(String::as_str) != Some(CONTRACT_VERSION)
            || cwd_tags.len() > 1
            || cwd_tags.iter().any(|tag| tag.len() != 2)
        {
            return Err(OmpkExecutionError::InvalidContract);
        }
        if harness_name != "ompk" && harness_name != "oh-my-pk" {
            return Err(OmpkExecutionError::UnsupportedRuntime);
        }

        let cwd = match cwd_tags.first() {
            Some(tag) => Some(self.validate_cwd(Path::new(&tag[1]))?),
            None => None,
        };
        Ok(Some(OmpkExecutionRequest {
            execution_id: event.id.to_hex(),
            cwd,
        }))
    }

    /// Parse one queued PKZZ turn deterministically.
    ///
    /// A batch may contain ordinary conversational events plus one execution
    /// request. More than one marked request is ambiguous because a single ACP
    /// session can have only one working directory, so it is rejected instead
    /// of choosing by arrival timing. Cancelled/prior events are deliberately
    /// not supplied by the caller and therefore cannot change new placement.
    pub fn parse_events<'a>(
        &self,
        events: impl IntoIterator<Item = &'a Event>,
        runtime_name: &str,
    ) -> Result<Option<OmpkExecutionRequest>, OmpkExecutionError> {
        let mut request = None;
        for event in events {
            if let Some(parsed) = self.parse_event(event, runtime_name)? {
                if request.is_some() {
                    return Err(OmpkExecutionError::InvalidContract);
                }
                request = Some(parsed);
            }
        }
        Ok(request)
    }

    fn validate_cwd(&self, requested: &Path) -> Result<PathBuf, OmpkExecutionError> {
        if !requested.is_absolute() || requested.as_os_str().is_empty() {
            return Err(OmpkExecutionError::InvalidCwd);
        }
        let canonical = requested
            .canonicalize()
            .map_err(|_| OmpkExecutionError::InvalidCwd)?;
        let canonical = normalize_windows_verbatim_path(canonical);
        if !canonical.is_dir() {
            return Err(OmpkExecutionError::InvalidCwd);
        }
        if !self
            .allowed_workspaces
            .iter()
            .any(|allowed| canonical.starts_with(allowed))
        {
            return Err(OmpkExecutionError::CwdNotAllowed);
        }
        Ok(canonical)
    }
}

/// ACP uses portable string paths. Windows canonicalization adds a `\\?\`
/// verbatim prefix that OMPK 16.4.1 does not accept when deriving its session
/// storage path, so remove only that representation prefix after filesystem
/// resolution. The resulting drive/UNC path remains absolute and canonical.
fn normalize_windows_verbatim_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let rendered = path.to_string_lossy();
        if let Some(rest) = rendered.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = rendered.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind, Tag};

    fn event(tags: Vec<Vec<&str>>) -> Event {
        let tags = tags
            .into_iter()
            .map(|parts| Tag::parse(parts).expect("valid test tag"))
            .collect::<Vec<_>>();
        EventBuilder::new(Kind::Custom(9), "work")
            .tags(tags)
            .sign_with_keys(&Keys::generate())
            .expect("sign test event")
    }

    #[test]
    fn ordinary_event_uses_existing_default_path() {
        let policy = OmpkExecutionPolicy::new(true, vec![]).expect("valid empty policy");
        assert_eq!(policy.parse_event(&event(vec![]), "ompk"), Ok(None));
    }

    #[test]
    fn explicit_allowed_cwd_is_canonicalized_and_correlated_to_event() {
        let root = tempfile::tempdir().expect("temp root");
        let child = root.path().join("repo");
        std::fs::create_dir(&child).expect("create child");
        let signed = event(vec![
            vec![EXECUTION_TAG, CONTRACT_VERSION],
            vec![CWD_TAG, child.to_str().expect("utf8 path")],
        ]);
        let policy =
            OmpkExecutionPolicy::new(true, vec![root.path().to_path_buf()]).expect("valid policy");
        let request = policy
            .parse_event(&signed, "oh-my-pk")
            .expect("valid request")
            .expect("execution request");
        assert_eq!(request.execution_id, signed.id.to_hex());
        assert_eq!(
            request.cwd,
            Some(normalize_windows_verbatim_path(
                child.canonicalize().expect("canonical")
            ))
        );
    }

    #[test]
    fn malformed_contract_is_rejected() {
        let root = tempfile::tempdir().expect("temp root");
        let raw = root.path().to_str().expect("utf8 path");
        let policy =
            OmpkExecutionPolicy::new(true, vec![root.path().to_path_buf()]).expect("valid policy");
        assert_eq!(
            policy.parse_event(&event(vec![vec![CWD_TAG, raw]]), "ompk"),
            Err(OmpkExecutionError::InvalidContract)
        );
        assert_eq!(
            policy.parse_event(
                &event(vec![
                    vec![EXECUTION_TAG, CONTRACT_VERSION],
                    vec![CWD_TAG, raw],
                    vec![CWD_TAG, raw],
                ]),
                "ompk",
            ),
            Err(OmpkExecutionError::InvalidContract)
        );
    }

    #[test]
    fn non_ompk_and_outside_allowlist_are_rejected_without_echoing_path() {
        let allowed = tempfile::tempdir().expect("allowed root");
        let outside = tempfile::tempdir().expect("outside root");
        let raw = outside.path().to_str().expect("utf8 path");
        let signed = event(vec![
            vec![EXECUTION_TAG, CONTRACT_VERSION],
            vec![CWD_TAG, raw],
        ]);
        let policy = OmpkExecutionPolicy::new(true, vec![allowed.path().to_path_buf()])
            .expect("valid policy");
        assert_eq!(
            policy.parse_event(&signed, "goose"),
            Err(OmpkExecutionError::UnsupportedRuntime)
        );
        let error = policy.parse_event(&signed, "ompk").unwrap_err();
        assert_eq!(error, OmpkExecutionError::CwdNotAllowed);
        assert!(!error.to_string().contains(raw));
    }

    #[test]
    fn disabled_policy_rejects_before_filesystem_validation() {
        let signed = event(vec![
            vec![EXECUTION_TAG, CONTRACT_VERSION],
            vec![CWD_TAG, "C:\\secret\\missing"],
        ]);
        let policy = OmpkExecutionPolicy::new(false, vec![]).expect("valid empty policy");
        assert_eq!(
            policy.parse_event(&signed, "ompk"),
            Err(OmpkExecutionError::Disabled)
        );
    }

    #[test]
    fn unsupported_machine_placement_is_rejected_explicitly() {
        let policy = OmpkExecutionPolicy::new(true, vec![]).expect("valid empty policy");
        let signed = event(vec![
            vec![EXECUTION_TAG, CONTRACT_VERSION],
            vec!["ompk-machine", "build-host"],
        ]);
        assert_eq!(
            policy.parse_event(&signed, "ompk"),
            Err(OmpkExecutionError::UnsupportedPlacement)
        );
    }

    #[test]
    fn multiple_marked_events_in_one_turn_are_ambiguous() {
        let policy = OmpkExecutionPolicy::new(true, vec![]).expect("valid empty policy");
        let first = event(vec![vec![EXECUTION_TAG, CONTRACT_VERSION]]);
        let second = event(vec![vec![EXECUTION_TAG, CONTRACT_VERSION]]);
        assert_eq!(
            policy.parse_events([&first, &second], "ompk"),
            Err(OmpkExecutionError::InvalidContract)
        );
    }

    #[test]
    fn invalid_allowlist_root_fails_closed() {
        assert!(matches!(
            OmpkExecutionPolicy::new(true, vec![PathBuf::from("relative")]),
            Err(OmpkExecutionError::InvalidAllowedWorkspace)
        ));
    }
}
