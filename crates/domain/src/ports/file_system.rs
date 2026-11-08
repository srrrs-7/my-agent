//! Filesystem port, expressed exclusively in sandboxed paths.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::FsError;
use crate::model::workspace::WorkspacePath;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub path: WorkspacePath,
    pub kind: EntryKind,
    pub size_bytes: u64,
}

#[async_trait]
pub trait FileSystem: Send + Sync {
    async fn read_to_string(&self, path: &WorkspacePath) -> Result<String, FsError>;

    /// Creates missing parent directories and overwrites existing content.
    async fn write(&self, path: &WorkspacePath, contents: &str) -> Result<(), FsError>;

    /// Non-recursive listing, sorted directories-first then alphabetically.
    async fn list_dir(&self, path: &WorkspacePath) -> Result<Vec<DirEntry>, FsError>;

    async fn exists(&self, path: &WorkspacePath) -> Result<bool, FsError>;

    /// Deletes the regular file at `path`, under the same rules as
    /// [`Self::write`]: a file that may not be written may not be deleted
    /// either. Refuses a directory, and a symbolic link - deleting the link
    /// and deleting what it points at are different requests, and guessing
    /// wrong loses a file.
    ///
    /// Parent directories are left in place, even when this empties them.
    ///
    /// No default: one that did nothing would let an undo report a file as
    /// deleted that is still there.
    async fn remove_file(&self, path: &WorkspacePath) -> Result<(), FsError>;

    /// The workspace path `path` really reaches once symbolic links are
    /// resolved - `Cargo.toml` for a `deps.txt` that links to it - or `path`
    /// itself when no link is involved. For a path that does not exist yet,
    /// the resolution of its deepest existing ancestor, followed by the rest.
    ///
    /// Fails, like a read or write would, when the path resolves outside the
    /// workspace or through a dangling link.
    ///
    /// No default, for the reason [`Tool::preview`] has none: a decorator
    /// that forgot to forward it would answer "no link" for everything, and
    /// every rule about which file is written could be walked around with an
    /// alias.
    ///
    /// [`Tool::preview`]: crate::ports::tool::Tool::preview
    async fn resolve_real(&self, path: &WorkspacePath) -> Result<WorkspacePath, FsError>;
}
