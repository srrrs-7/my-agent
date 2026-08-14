//! System-prompt construction port.
//!
//! How the ambient context and the tool list are rendered into a system prompt
//! is a *policy*, and one the roadmap already plans to vary (operator-supplied
//! prompts, per-task variants). Making it a port keeps the loop ignorant of the
//! rendering and lets the composition root swap or decorate it - exactly like
//! every other seam in this crate.
//!
//! The trait is synchronous on purpose: building a prompt is pure string work.
//! Anything that needs IO to *gather* information belongs in
//! [`super::context::ContextProvider`], which runs before this port is called.

use crate::model::context::ContextSnapshot;
use crate::model::tool::ToolDefinition;

/// Renders the system prompt for one run.
///
/// Implementations must be deterministic for a given input: the loop builds
/// the prompt once per run and provider-side prompt caching relies on it being
/// stable across iterations.
pub trait PromptBuilder: Send + Sync {
    fn build(&self, context: &ContextSnapshot, tools: &[ToolDefinition]) -> String;
}
