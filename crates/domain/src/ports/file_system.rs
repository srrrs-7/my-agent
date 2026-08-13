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
}
