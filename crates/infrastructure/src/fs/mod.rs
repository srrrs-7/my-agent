//! Filesystem adapters, all confined to the workspace root.

pub mod context;
pub mod local;
pub mod scratch;
pub mod search;

pub use context::WorkspaceContextProvider;
pub use local::LocalFileSystem;
pub use scratch::{ScratchDir, ScratchError};
pub use search::IgnoreAwareSearcher;

use agent_domain::error::FsError;
use agent_domain::model::workspace::WorkspaceRoot;

/// The workspace as the filesystem sees it: the same access policy, at the
/// symlink-resolved location that every canonical path starts with.
///
/// The access policy is applied a second time against this root, to the path
/// a request really reaches. Comparing a canonical path with the root as
/// configured would fail whenever the root itself sits behind a symlink
/// (`/tmp` on macOS), refusing everything for the wrong reason.
pub(crate) fn canonical_root(root: &WorkspaceRoot) -> Result<WorkspaceRoot, FsError> {
    let canonical = std::fs::canonicalize(root.as_path()).map_err(|error| FsError::Io {
        path: root.display(),
        message: format!("cannot open the workspace root: {error}"),
    })?;
    Ok(WorkspaceRoot::new(canonical)?.with_policy(root.policy().clone()))
}
