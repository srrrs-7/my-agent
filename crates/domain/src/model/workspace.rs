//! The workspace sandbox.
//!
//! Confining file access to one directory is a *business rule* of this agent,
//! not an implementation detail of some adapter, so it is enforced here. An
//! adapter cannot touch a file without first obtaining a [`WorkspacePath`],
//! and the only way to obtain one is through [`WorkspaceRoot::resolve`].
//!
//! Adapters are still expected to re-check after canonicalisation, because
//! symlinks can only be resolved by touching the filesystem (see
//! `agent-infrastructure::fs::local`).

use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::DomainError;

/// A path that is guaranteed to be relative to the workspace root, normalised,
/// and free of any `..` component that could climb out of it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkspacePath(PathBuf);

impl WorkspacePath {
    /// The workspace root itself.
    pub fn root() -> Self {
        Self(PathBuf::new())
    }

    /// Normalises a *relative* path. Prefer [`WorkspaceRoot::resolve`], which
    /// additionally accepts absolute paths that live inside the root.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        let raw = raw.trim();
        if raw.contains('\0') {
            return Err(DomainError::invalid("path", "must not contain NUL bytes"));
        }
        let candidate = Path::new(raw);
        if candidate.is_absolute() {
            return Err(DomainError::PathEscape {
                path: raw.to_string(),
            });
        }
        Self::normalize(candidate, raw)
    }

    fn normalize(candidate: &Path, original: &str) -> Result<Self, DomainError> {
        let mut normalized = PathBuf::new();
        for component in candidate.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(segment) => normalized.push(segment),
                Component::ParentDir => {
                    if !normalized.pop() {
                        return Err(DomainError::PathEscape {
                            path: original.to_string(),
                        });
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(DomainError::PathEscape {
                        path: original.to_string(),
                    });
                }
            }
        }
        Ok(Self(normalized))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.as_os_str().is_empty()
    }

    pub fn join(&self, segment: &str) -> Result<Self, DomainError> {
        let joined = self.0.join(segment);
        Self::normalize(&joined, &joined.to_string_lossy())
    }

    /// Stable display form: always `/`-separated, `.` for the root.
    pub fn display(&self) -> String {
        if self.is_root() {
            ".".to_string()
        } else {
            self.0
                .components()
                .filter_map(|component| match component {
                    Component::Normal(segment) => Some(segment.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/")
        }
    }
}

impl TryFrom<String> for WorkspacePath {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<WorkspacePath> for String {
    fn from(value: WorkspacePath) -> Self {
        value.display()
    }
}

impl fmt::Display for WorkspacePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// The absolute directory every [`WorkspacePath`] is interpreted against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRoot {
    path: PathBuf,
}

impl WorkspaceRoot {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, DomainError> {
        let path = path.into();
        if !path.is_absolute() {
            return Err(DomainError::invalid(
                "workspace root",
                format!("`{}` must be an absolute path", path.display()),
            ));
        }
        Ok(Self { path })
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }

    pub fn display(&self) -> String {
        self.path.display().to_string()
    }

    /// Turns whatever the model wrote into a sandboxed path.
    ///
    /// Accepts relative paths (`src/main.rs`, `./src`, `` ) and absolute paths
    /// that are inside the root (`/workspace/src/main.rs`); everything else is
    /// refused with [`DomainError::PathEscape`].
    pub fn resolve(&self, raw: &str) -> Result<WorkspacePath, DomainError> {
        let trimmed = raw.trim();
        if trimmed.contains('\0') {
            return Err(DomainError::invalid("path", "must not contain NUL bytes"));
        }
        if trimmed.is_empty() {
            return Ok(WorkspacePath::root());
        }

        let candidate = Path::new(trimmed);
        if candidate.is_absolute() {
            let relative =
                candidate
                    .strip_prefix(&self.path)
                    .map_err(|_| DomainError::PathEscape {
                        path: trimmed.to_string(),
                    })?;
            return WorkspacePath::normalize(relative, trimmed);
        }
        WorkspacePath::normalize(candidate, trimmed)
    }

    /// Absolute location of a sandboxed path. Lexically safe by construction;
    /// adapters must still canonicalise to defeat symlinks.
    pub fn absolute(&self, path: &WorkspacePath) -> PathBuf {
        if path.is_root() {
            self.path.clone()
        } else {
            self.path.join(path.as_path())
        }
    }

    /// Inverse of [`Self::absolute`], used when an adapter discovered a file by
    /// walking the tree.
    pub fn relativize(&self, absolute: &Path) -> Result<WorkspacePath, DomainError> {
        let relative = absolute
            .strip_prefix(&self.path)
            .map_err(|_| DomainError::PathEscape {
                path: absolute.display().to_string(),
            })?;
        WorkspacePath::normalize(relative, &absolute.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> WorkspaceRoot {
        WorkspaceRoot::new("/workspace").unwrap()
    }

    #[test]
    fn root_must_be_absolute() {
        assert!(WorkspaceRoot::new("relative/dir").is_err());
    }

    #[test]
    fn resolves_relative_paths() {
        let root = root();
        assert_eq!(
            root.resolve("src/main.rs").unwrap().display(),
            "src/main.rs"
        );
        assert_eq!(
            root.resolve("./src/./main.rs").unwrap().display(),
            "src/main.rs"
        );
        assert_eq!(
            root.resolve("src/../src/main.rs").unwrap().display(),
            "src/main.rs"
        );
    }

    #[test]
    fn resolves_absolute_paths_inside_the_root() {
        let root = root();
        assert_eq!(
            root.resolve("/workspace/src/main.rs").unwrap().display(),
            "src/main.rs"
        );
    }

    #[test]
    fn empty_and_dot_mean_the_root() {
        let root = root();
        assert!(root.resolve("").unwrap().is_root());
        assert!(root.resolve(".").unwrap().is_root());
        assert_eq!(root.resolve(".").unwrap().display(), ".");
    }

    #[test]
    fn refuses_traversal() {
        let root = root();
        assert!(root.resolve("../etc/passwd").is_err());
        assert!(root.resolve("src/../../etc/passwd").is_err());
        assert!(root.resolve("/etc/passwd").is_err());
        assert!(root.resolve("/workspace/../etc/passwd").is_err());
    }

    #[test]
    fn refuses_nul_bytes() {
        assert!(root().resolve("src/main\0.rs").is_err());
    }

    #[test]
    fn absolute_round_trips() {
        let root = root();
        let path = root.resolve("src/main.rs").unwrap();
        let absolute = root.absolute(&path);
        assert_eq!(absolute, PathBuf::from("/workspace/src/main.rs"));
        assert_eq!(root.relativize(&absolute).unwrap(), path);
    }

    #[test]
    fn absolute_of_root_is_the_root() {
        let root = root();
        assert_eq!(
            root.absolute(&WorkspacePath::root()),
            PathBuf::from("/workspace")
        );
    }
}
