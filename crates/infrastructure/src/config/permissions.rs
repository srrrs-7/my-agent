//! The permission rules file (`AGENT_PERMISSIONS`).
//!
//! A rule can spare the human a question (`allow`), so the file is the
//! operator's and only the operator's, on the same terms as the MCP
//! configuration: named by an absolute path in the real environment (never
//! `.env`, see `agent-cli`'s `dotenv`), and refused when it lives inside the
//! workspace, where the repository or the model could rewrite it. A rules
//! file inside the workspace is not read in this version at all.
//!
//! ```json
//! {
//!   "rules": [
//!     { "tool": "write_file", "path": "migrations/", "effect": "deny" },
//!     { "tool": "edit_file", "path": "src/", "effect": "allow" },
//!     { "tool": "web_fetch", "domain": "docs.rs", "effect": "allow" },
//!     { "tool": "run_command", "command": "git push", "effect": "ask" }
//!   ]
//! }
//! ```
//!
//! Unknown fields are errors, so a misspelt `efect` cannot fall back to
//! something the operator did not write.

use std::path::{Path, PathBuf};

use agent_domain::error::DomainError;
use agent_domain::model::access::PathPattern;
use agent_domain::model::permission::{
    CommandPrefix, DomainPattern, Effect, PermissionRule, PermissionRules, SubjectPattern,
};
use agent_domain::model::tool::ToolName;
use serde::Deserialize;
use thiserror::Error;

use super::ConfigError;
use super::env::Reader;
use crate::mcp::config::refuse_inside;

pub(super) const PERMISSIONS_KEY: &str = "AGENT_PERMISSIONS";

/// Larger than any hand-written rules file needs.
pub const MAX_PERMISSIONS_BYTES: u64 = 256 * 1024;
/// Every call is compared with every rule of its tool.
pub const MAX_RULES: usize = 512;

/// `AGENT_PERMISSIONS`, checked for shape only: an absolute path. The file is
/// read by [`load_permissions`] once the workspace is known.
pub(super) fn read_permissions_path(reader: &Reader<'_>) -> Result<Option<PathBuf>, ConfigError> {
    match reader.string(PERMISSIONS_KEY) {
        None => Ok(None),
        Some(raw) if Path::new(&raw).is_absolute() => Ok(Some(PathBuf::from(raw))),
        Some(raw) => Err(ConfigError::invalid(
            PERMISSIONS_KEY,
            raw,
            "must be an absolute path outside the workspace (`~` is not expanded), e.g. \
             /home/me/.config/agent/permissions.json",
        )),
    }
}

#[derive(Debug, Error)]
pub enum PermissionsError {
    #[error("cannot read the permission rules `{path}`: {message}")]
    Unreadable { path: String, message: String },

    #[error("the permission rules `{path}` are larger than {limit} bytes")]
    TooLarge { path: String, limit: u64 },

    #[error("{0}")]
    InsideWorkspace(String),

    #[error("the permission rules are not valid: {message}")]
    Parse { message: String },

    #[error("permission rule {index} (`{tool}`): {reason}")]
    Invalid {
        index: usize,
        tool: String,
        reason: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    rules: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    tool: String,
    effect: RawEffect,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    command: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawEffect {
    Allow,
    Ask,
    Deny,
}

/// Reads and parses the file at `path`, refusing it when it resolves to a
/// place inside the canonical `workspace`.
///
/// Only the file's own shape is checked here. Whether each rule fits its
/// tool (a `domain` on a file tool, an `allow` on `run_command`) needs the
/// tools, and is checked where the rules are put in front of them.
pub fn load_permissions(
    path: &Path,
    workspace: &Path,
) -> Result<PermissionRules, PermissionsError> {
    let unreadable = |error: std::io::Error| PermissionsError::Unreadable {
        path: path.display().to_string(),
        message: error.to_string(),
    };
    let resolved = std::fs::canonicalize(path).map_err(unreadable)?;
    refuse_inside(workspace, &resolved, "the permission rules")
        .map_err(|error| PermissionsError::InsideWorkspace(error.to_string()))?;

    let size = std::fs::metadata(&resolved).map_err(unreadable)?.len();
    if size > MAX_PERMISSIONS_BYTES {
        return Err(PermissionsError::TooLarge {
            path: path.display().to_string(),
            limit: MAX_PERMISSIONS_BYTES,
        });
    }
    parse_permissions(&std::fs::read_to_string(&resolved).map_err(unreadable)?)
}

pub fn parse_permissions(text: &str) -> Result<PermissionRules, PermissionsError> {
    let file: File = serde_json::from_str(text).map_err(|error| PermissionsError::Parse {
        message: error.to_string(),
    })?;
    if file.rules.len() > MAX_RULES {
        return Err(PermissionsError::Parse {
            message: format!(
                "{} rules, more than the {MAX_RULES} allowed",
                file.rules.len()
            ),
        });
    }
    file.rules
        .into_iter()
        .enumerate()
        .map(|(index, raw)| {
            let tool = raw.tool.clone();
            rule(raw).map_err(|reason| PermissionsError::Invalid {
                // Counted from 1, as a person reading the file counts.
                index: index + 1,
                tool,
                reason,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(PermissionRules::new)
}

fn rule(raw: RawRule) -> Result<PermissionRule, String> {
    let reason = |error: DomainError| match error {
        DomainError::Invalid { reason, .. } => reason,
        other => other.to_string(),
    };
    let tool = ToolName::new(raw.tool).map_err(reason)?;
    let subject = match (raw.path, raw.domain, raw.command) {
        (None, None, None) => None,
        (Some(path), None, None) => Some(SubjectPattern::Path(
            PathPattern::parse(&path).map_err(reason)?,
        )),
        (None, Some(domain), None) => Some(SubjectPattern::Domain(
            DomainPattern::parse(&domain).map_err(reason)?,
        )),
        (None, None, Some(command)) => Some(SubjectPattern::Command(
            CommandPrefix::parse(&command).map_err(reason)?,
        )),
        _ => {
            return Err(
                "give at most one of `path`, `domain` and `command` - write two rules to cover \
                 two"
                .to_string(),
            );
        }
    };
    Ok(PermissionRule {
        tool,
        subject,
        effect: match raw.effect {
            RawEffect::Allow => Effect::Allow,
            RawEffect::Ask => Effect::Ask,
            RawEffect::Deny => Effect::Deny,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    const RULES: &str = r#"{"rules": [
        {"tool": "write_file", "path": "migrations/", "effect": "deny"},
        {"tool": "web_fetch", "domain": "docs.rs", "effect": "allow"},
        {"tool": "run_command", "command": "git push", "effect": "ask"},
        {"tool": "web_search", "effect": "ask"}
    ]}"#;

    #[test]
    fn every_kind_of_rule_parses() {
        let rules = parse_permissions(RULES).unwrap();
        let shown: Vec<String> = rules.rules().iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            [
                "deny write_file path `migrations/`",
                "allow web_fetch domain `docs.rs`",
                "ask run_command command `git push`",
                "ask web_search",
            ]
        );
    }

    #[test]
    fn unknown_fields_and_effects_are_errors() {
        for text in [
            r#"{"rules": [{"tool": "read_file", "efect": "deny"}]}"#,
            r#"{"rules": [{"tool": "read_file", "effect": "never"}]}"#,
            r#"{"rules": [], "extra": true}"#,
            r#"{"rules": [{"tool": "read_file", "effect": "Deny"}]}"#,
        ] {
            assert!(
                matches!(parse_permissions(text), Err(PermissionsError::Parse { .. })),
                "{text}"
            );
        }
    }

    #[test]
    fn a_bad_rule_is_reported_by_its_position() {
        let error = parse_permissions(
            r#"{"rules": [
                {"tool": "read_file", "effect": "deny"},
                {"tool": "read_file", "path": "**/*.pem", "effect": "deny"}
            ]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("rule 2 (`read_file`)"), "{error}");
        assert!(error.contains("`**`"), "{error}");

        let two = parse_permissions(
            r#"{"rules": [{"tool": "web_fetch", "path": "a", "domain": "b.c", "effect": "deny"}]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(two.contains("at most one"), "{two}");
    }

    #[test]
    fn a_rules_file_inside_the_workspace_is_refused() {
        let (_dir, workspace) = canonical_tempdir();
        std::fs::create_dir(workspace.join(".agent")).unwrap();
        let inside = workspace.join(".agent/permissions.json");
        std::fs::write(&inside, RULES).unwrap();

        let error = load_permissions(&inside, &workspace).unwrap_err();
        assert!(
            matches!(error, PermissionsError::InsideWorkspace(_)),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("inside the workspace"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_from_outside_into_the_workspace_is_refused_by_where_it_leads() {
        let (_workspace_dir, workspace) = canonical_tempdir();
        let (_outside_dir, outside) = canonical_tempdir();
        std::fs::write(workspace.join("rules.json"), RULES).unwrap();
        let link = outside.join("permissions.json");
        std::os::unix::fs::symlink(workspace.join("rules.json"), &link).unwrap();

        assert!(matches!(
            load_permissions(&link, &workspace),
            Err(PermissionsError::InsideWorkspace(_))
        ));
    }

    #[test]
    fn a_rules_file_outside_the_workspace_is_read() {
        let (_workspace_dir, workspace) = canonical_tempdir();
        let (_outside_dir, outside) = canonical_tempdir();
        let path = outside.join("permissions.json");
        std::fs::write(&path, RULES).unwrap();

        assert_eq!(
            load_permissions(&path, &workspace).unwrap().rules().len(),
            4
        );
    }
}
