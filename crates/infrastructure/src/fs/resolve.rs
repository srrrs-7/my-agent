//! The workspace's access policy as the filesystem applies it.
//!
//! [`WorkspacePath`] is checked lexically, so a symlink inside the workspace
//! can lead anywhere: out of it, or onto a file the policy protects under an
//! innocent name. Every adapter that touches files therefore checks twice -
//! once for the name the caller used, once for the file it really reaches -
//! and this is the one place both checks are written.
//!
//! Comparing a canonical path with the root as configured would fail whenever
//! the root itself sits behind a symlink (`/tmp` on macOS), refusing
//! everything for the wrong reason. So the second check runs against the
//! symlink-resolved root, which is held next to the configured one.

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use agent_domain::error::FsError;
use agent_domain::model::access::FileAccess;
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};

/// The workspace as configured and at the symlink-resolved location every
/// canonical path starts with, under the same access policy.
pub(crate) struct ResolvedWorkspace {
    root: WorkspaceRoot,
    real_root: WorkspaceRoot,
}

impl ResolvedWorkspace {
    pub(crate) fn new(root: &WorkspaceRoot) -> Result<Self, FsError> {
        let canonical = std::fs::canonicalize(root.as_path()).map_err(|error| FsError::Io {
            path: root.display(),
            message: format!("cannot open the workspace root: {error}"),
        })?;
        let real_root = WorkspaceRoot::new(canonical)?.with_policy(root.policy().clone());
        Ok(Self {
            root: root.clone(),
            real_root,
        })
    }

    /// The symlink-resolved root.
    pub(crate) fn real_root(&self) -> &WorkspaceRoot {
        &self.real_root
    }

    /// The lexical check alone: the name `requested` was asked for by.
    ///
    /// Run before anything is resolved: `.env -> config/app.env` is refused
    /// here, since its target has an innocent name.
    pub(crate) fn check_requested(
        &self,
        requested: &WorkspacePath,
        access: FileAccess,
    ) -> Result<(), FsError> {
        if self.root.permits(requested, access) {
            Ok(())
        } else {
            Err(denied(requested, access))
        }
    }

    /// Both checks: the name `requested` was asked for by, and `real`, the
    /// canonical path it resolves to. `notes -> .env` is refused by the
    /// second, since the name it was asked for by is innocent.
    ///
    /// Errors always name `requested`, never where a link leads.
    pub(crate) fn check(
        &self,
        requested: &WorkspacePath,
        real: &Path,
        access: FileAccess,
    ) -> Result<(), FsError> {
        let real = self
            .real_root
            .relativize(real)
            .map_err(|_| FsError::OutsideWorkspace {
                path: requested.display(),
            })?;
        if self.root.permits(requested, access) && self.real_root.permits(&real, access) {
            Ok(())
        } else {
            Err(denied(requested, access))
        }
    }

    /// Absolute, symlink-resolved location of `path`, or
    /// [`FsError::OutsideWorkspace`] if it leaves the sandbox, or
    /// [`FsError::AccessDenied`] if `access` is given and the policy refuses
    /// either the path or what it resolves to.
    ///
    /// Works for paths that do not exist yet (needed for `write_file`), by
    /// [`resolve_leniently`]: the tail appended to the resolved ancestor
    /// contains no `..`, because a [`WorkspacePath`] cannot.
    pub(crate) fn resolve(
        &self,
        path: &WorkspacePath,
        access: Option<FileAccess>,
    ) -> Result<PathBuf, FsError> {
        if let Some(access) = access {
            self.check_requested(path, access)?;
        }

        let resolved =
            resolve_leniently(&self.root.absolute(path)).map_err(|error| FsError::Io {
                path: path.display(),
                message: if error.kind() == ErrorKind::NotFound {
                    // The ancestor was found by `lstat`, so short of a race,
                    // only a symlink whose target is missing gets here.
                    // Writing through it would create whatever it names,
                    // inside the workspace or not.
                    "it is a symbolic link to a path that does not exist, so it was not followed - \
                     use a regular path instead"
                        .to_string()
                } else {
                    error.to_string()
                },
            })?;

        // Checked on the ancestor that exists, not on the joined path: if the
        // workspace root itself is gone, the ancestor is above it, and the
        // tail would otherwise spell the root back into the answer.
        if !resolved.existing.starts_with(self.real_root.as_path()) {
            return Err(FsError::OutsideWorkspace {
                path: path.display(),
            });
        }

        if let Some(access) = access {
            self.check(path, &resolved.path, access)?;
        }
        Ok(resolved.path)
    }
}

fn denied(path: &WorkspacePath, access: FileAccess) -> FsError {
    FsError::AccessDenied {
        path: path.display(),
        access,
    }
}

/// Where [`resolve_leniently`] took a path.
pub(crate) struct Resolution {
    /// The deepest ancestor that exists, symlinks resolved.
    pub(crate) existing: PathBuf,
    /// `existing` with the part that does not exist yet appended as written.
    pub(crate) path: PathBuf,
}

/// `path` with symlinks resolved, including when it does not exist yet: the
/// deepest existing ancestor is canonicalised and the rest appended as
/// written. The caller makes sure that rest holds no `..`, which would mean
/// something different once the ancestor is resolved.
///
/// The ancestor is found with `lstat` rather than `exists`: `exists` follows
/// symlinks, so a symlink whose target is missing would count as absent and
/// be written *through* - creating its target wherever it points. Such a link
/// is the ancestor, and canonicalising it fails with `NotFound`.
///
/// A relative path none of whose ancestors exists fails with `NotFound` too.
pub(crate) fn resolve_leniently(path: &Path) -> io::Result<Resolution> {
    let mut ancestor = path;
    while std::fs::symlink_metadata(ancestor).is_err() {
        ancestor = ancestor.parent().ok_or_else(|| {
            io::Error::new(
                ErrorKind::NotFound,
                format!("no part of `{}` exists", path.display()),
            )
        })?;
    }

    let existing = std::fs::canonicalize(ancestor)?;
    let tail = path.strip_prefix(ancestor).unwrap_or(Path::new(""));
    let path = if tail.as_os_str().is_empty() {
        // Joining an empty path appends a trailing separator, and a trailing
        // slash on a regular file makes every syscall fail with ENOTDIR. The
        // path already exists, so there is nothing to append.
        existing.clone()
    } else {
        existing.join(tail)
    };
    Ok(Resolution { existing, path })
}
