//! Search port: by content ([`FileSearcher::search`]) and by path
//! ([`FileSearcher::glob`]).
//!
//! Separate from [`super::file_system::FileSystem`] on purpose: walking a tree
//! while honouring `.gitignore` and matching patterns is a different capability
//! with different dependencies, and most fakes only need one of the two. Both
//! searches walk the same tree under the same rules (ignore files, the access
//! policy, no symlinks followed), which is why they share one port.

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobQuery {
    /// Glob matched against paths relative to `root`, in `.gitignore`
    /// syntax: `*.rs` matches at any depth, `src/**/*.rs` is anchored at
    /// `root`. Never absolute and never containing `..`.
    pub pattern: String,
    /// Subtree to search.
    pub root: WorkspacePath,
    pub max_results: usize,
}

/// Files whose path matched a [`GlobQuery`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GlobMatches {
    /// Most recently modified first, ties broken by path, at most
    /// `max_results` of them. Recency first because the file someone just
    /// touched is the likeliest one to matter next.
    pub paths: Vec<WorkspacePath>,
    /// How many files matched before the cap was applied. Larger than
    /// `paths.len()` exactly when the list was truncated.
    pub total: usize,
}

impl GlobMatches {
    pub fn truncated(&self) -> bool {
        self.total > self.paths.len()
    }
}

#[async_trait]
pub trait FileSearcher: Send + Sync {
    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchHit>, FsError>;

    /// Files whose path matches `query.pattern`. Directories are never
    /// returned; protected and ignored files are not either.
    async fn glob(&self, query: GlobQuery) -> Result<GlobMatches, FsError>;
}
