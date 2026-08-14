//! Tool implementations.
//!
//! Each tool is a use case: it validates the model's arguments, drives one or
//! more ports, and renders a result that is useful *to a language model* -
//! which usually means "short, unambiguous, and explicit about what to do
//! next when something went wrong".

pub mod file;
pub mod registry;
pub mod util;
pub mod web;

pub use registry::ToolRegistry;
