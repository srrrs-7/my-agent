//! Content search port.
//!
//! Separate from [`super::file_system::FileSystem`] on purpose: walking a tree
//! while honouring `.gitignore` and matching regexes is a different capability
//! with different dependencies, and most fakes only need one of the two.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::FsError;
use crate::model::workspace::WorkspacePath;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// Regular expression matched against each line.
    pub pattern: String,
    /// Subtree to search.
    pub root: WorkspacePath,
    /// Optional glob restricting which files are considered (`**/*.rs`).
    pub include_glob: Option<String>,
    pub case_insensitive: bool,
    pub max_results: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub path: WorkspacePath,
    pub line_number: u64,
    pub line: String,
}

#[async_trait]
pub trait FileSearcher: Send + Sync {
    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchHit>, FsError>;
}
