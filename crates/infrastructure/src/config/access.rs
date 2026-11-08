//! Which files the file tools refuse, and where the session log sits.
//!
//! The session log holds the conversation verbatim, so when it lies inside
//! the workspace it is added to the access policy. Deciding whether it does
//! means comparing two paths that may each be spelled with `..` or reached
//! through a symlink, which is the path resolution kept here.

use std::path::{Component, Path, PathBuf};

use agent_domain::error::DomainError;
use agent_domain::model::access::{AccessPolicy, PathPattern};
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};

use super::env::Reader;
use super::{ConfigError, Settings};

/// Files inside the workspace the file tools refuse, on top of the built-in
/// rules ([`AccessPolicy::standard`]) and the session log directory.
///
/// Additions only. There is deliberately no setting that removes a built-in
/// rule: those name files that are never project files, and a project that
/// needs a value from one can put it in the prompt.
#[derive(Debug, Clone, Default)]
pub struct AccessSettings {
    /// Neither read nor written.
    pub deny_read: Vec<PathPattern>,
    /// Readable, but never written.
    pub deny_write: Vec<PathPattern>,
}

const DENY_READ_KEY: &str = "AGENT_DENY_READ";
const DENY_WRITE_KEY: &str = "AGENT_DENY_WRITE";

impl AccessSettings {
    pub(super) fn read(reader: &Reader<'_>) -> Result<Self, ConfigError> {
        Ok(Self {
            deny_read: patterns(reader, DENY_READ_KEY)?,
            deny_write: patterns(reader, DENY_WRITE_KEY)?,
        })
    }
}

/// A pattern that does not parse aborts startup. Skipping it would leave the
/// operator believing a file is protected that is not.
fn patterns(reader: &Reader<'_>, key: &str) -> Result<Vec<PathPattern>, ConfigError> {
    reader
        .list(key)
        .iter()
        .map(|raw| {
            PathPattern::parse(raw).map_err(|error| {
                let reason = match error {
                    DomainError::Invalid { reason, .. } => reason,
                    other => other.to_string(),
                };
                ConfigError::invalid(
                    key,
                    raw,
                    format!(
                        "{reason}. Expected names such as `*.p12` or directories such as `secrets/`"
                    ),
                )
            })
        })
        .collect()
}

impl Settings {
    /// The rules the file tools enforce in a workspace at `workspace`.
    ///
    /// The workspace is passed in rather than read from `self.workspace`
    /// because the composition root has the canonical path, and the session
    /// directory has to be compared with the path files are really reached
    /// by.
    pub fn access_policy(&self, workspace: &Path) -> Result<AccessPolicy, ConfigError> {
        let mut policy = AccessPolicy::standard();

        // Protected whether or not logging is on: turning the log off stops
        // new records, it does not remove the old ones.
        if let Some(sessions) = self.session_dir_inside(workspace) {
            let pattern = PathPattern::directory(&sessions).map_err(|_| {
                ConfigError::invalid(
                    "AGENT_SESSION_DIR",
                    self.session.dir.display().to_string(),
                    "the session log cannot be the workspace root itself: it holds the whole \
                     conversation, and keeping it from the file tools would refuse every file. \
                     Use a subdirectory such as .agent/sessions, or a path outside the workspace",
                )
            })?;
            policy = policy.with_read_denied(pattern);
        }

        for pattern in &self.access.deny_read {
            policy = policy.with_read_denied(pattern.clone());
        }
        for pattern in &self.access.deny_write {
            policy = policy.with_write_denied(pattern.clone());
        }
        Ok(policy)
    }

    /// Where the session log sits relative to `workspace`, or `None` when it
    /// is outside.
    ///
    /// Compared both as written and as resolved, so that neither a `..` in
    /// the setting nor a symlink between the two spellings of the workspace
    /// hides that the log is inside.
    fn session_dir_inside(&self, workspace: &Path) -> Option<WorkspacePath> {
        let dir = if self.session.dir.is_absolute() {
            self.session.dir.clone()
        } else {
            workspace.join(&self.session.dir)
        };
        let spellings = |path: &Path| {
            [Some(lexically_normal(path)), resolved(path)]
                .into_iter()
                .flatten()
        };

        spellings(workspace)
            .filter_map(|root| WorkspaceRoot::new(root).ok())
            .find_map(|root| spellings(&dir).find_map(|dir| root.relativize(&dir).ok()))
    }

    /// Absolute location of the session directory.
    ///
    /// A relative setting is read against the workspace rather than against
    /// the current directory, so `agent` started from a subdirectory writes to
    /// the same place as one started from the root.
    pub fn session_dir(&self) -> PathBuf {
        if self.session.dir.is_absolute() {
            self.session.dir.clone()
        } else {
            self.workspace.join(&self.session.dir)
        }
    }
}

/// `path` with `.` and `..` resolved by text alone. `..` above the root
/// stays at the root, as it does on the filesystem.
fn lexically_normal(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

/// `path` with symlinks resolved, including when it does not exist yet - the
/// session directory is only created by the first session.
fn resolved(path: &Path) -> Option<PathBuf> {
    crate::fs::resolve::resolve_leniently(&lexically_normal(path))
        .ok()
        .map(|resolution| resolution.path)
}
