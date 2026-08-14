//! Command execution.
//!
//! One tool, and the only one that hands work to a process this agent does not
//! control. Its safety class is
//! [`agent_domain::model::tool::ToolSafety::Destructive`] for that reason
//! alone: what a command does cannot be read off its arguments, so the human
//! approves the literal shell line before it runs.
//!
//! The confinement is the runner's job, not this module's - see
//! [`agent_domain::ports::command`]. What lives here is the translation
//! between a model's arguments and that port, and the rendering of a result
//! that tells the model what actually happened.

mod run;

pub use run::RunCommandTool;
