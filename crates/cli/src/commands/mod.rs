//! One module per subcommand.
//!
//! Each takes the already-wired [`crate::composition::Application`] and does
//! nothing but drive it and print, which keeps `main` down to argument parsing
//! and dispatch.

pub mod chat;
mod checkpoint;
mod context;
pub mod doctor;
pub mod eval;
mod plan_mode;
pub mod run;
pub mod sessions;
pub mod tools;
mod turn;
