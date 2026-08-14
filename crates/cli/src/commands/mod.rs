//! One module per subcommand.
//!
//! Each takes the already-wired [`crate::composition::Application`] and does
//! nothing but drive it and print, which keeps `main` down to argument parsing
//! and dispatch.

pub mod chat;
pub mod doctor;
pub mod run;
pub mod sessions;
pub mod tools;
