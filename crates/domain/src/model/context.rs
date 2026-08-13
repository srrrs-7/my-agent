//! Ambient context handed to the model at the start of a run.
//!
//! This is the "context engineering" payload: everything the agent knows about
//! its environment before the user says anything. Gathering it requires IO, so
//! it arrives through [`crate::ports::context::ContextProvider`]; the shape
//! itself is domain vocabulary.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ContextSnapshot {
    /// Absolute path of the sandbox root, as the model should refer to it.
    pub workspace_root: String,
    pub os: String,
    /// ISO-8601 date, so the model does not guess "today".
    pub today: String,
    pub is_git_repository: bool,
    /// Contents of a project instruction file (`AGENTS.md`, `CLAUDE.md`, ...).
    pub project_instructions: Option<String>,
    /// A shallow listing of the workspace so the model can orient itself
    /// without spending a turn on `list_directory`.
    pub directory_overview: Vec<String>,
}
