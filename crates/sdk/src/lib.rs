//! # agent-sdk
//!
//! The `agent` coding agent as a library: the same wiring, tools, sandboxes
//! and approval stack as the binary, behind a small API.
//!
//! ```no_run
//! # async fn example() -> Result<(), agent_sdk::Error> {
//! use agent_sdk::{Agent, Config, approval};
//!
//! let config = Config::from_env()?.with_workspace("/path/to/project");
//! let agent = Agent::builder(config)
//!     .approval(approval::from_fn(|request| {
//!         // Put the question to your user here.
//!         approval::ApprovalDecision::Deny {
//!             reason: format!("`{}` is not allowed here", request.call.name),
//!         }
//!     }))
//!     .build()
//!     .await?;
//! let outcome = agent.run("Summarise the README").await?;
//! println!("{}", outcome.final_text);
//! # Ok(())
//! # }
//! ```
//!
//! ## Where it sits
//!
//! ```text
//!   agent-cli ............ the binary: argv, terminal, REPL, .env
//!     -> agent-sdk ........ this crate: the composition root
//!          -> agent-application     the loop, the tools, prompt assembly
//!          -> agent-infrastructure  HTTP clients, filesystem, config
//!               -> agent-domain     entities, value objects, ports
//! ```
//!
//! Every agent - each subcommand of the binary, an eval attempt, an
//! embedder's [`Agent`] - is wired by [`unstable::assemble`]. The binary adds
//! only what belongs to a terminal: its approval prompt, its renderer, its
//! notifications, Ctrl-C and the REPL.
//!
//! ## Stability
//!
//! **Stable** - kept compatible across releases; a breaking change is a
//! major version:
//!
//! - [`Agent`], [`AgentBuilder`], [`Session`], [`Interrupt`], [`Outcome`],
//!   [`Reason`], [`Config`], [`Error`]
//! - [`events`]: the JSON shapes of events and results, versioned by
//!   [`events::SCHEMA_VERSION`] - the same lines `agent run --output-format
//!   stream-json` prints
//! - [`approval::from_fn`], and the presence of the re-exported ports
//!   [`ApprovalGate`] and [`EventSink`]
//! - the `AGENT_*` variables [`Config`] reads (README §8)
//!
//! Public enums and the result type are `#[non_exhaustive]`: a new reason or
//! event is not a breaking change, so match with a wildcard arm.
//!
//! **Not stable**:
//!
//! - [`unstable`]: the composition root itself, for the binary and for tests
//!   that need to see the real wiring. It names the inner crates' types and
//!   changes with them.
//! - Items marked `#[doc(hidden)]` (`Agent::assembly`, `Config::settings`).
//! - The fields of the domain types the re-exported ports carry
//!   ([`AgentEvent`], [`approval::ApprovalRequest`] and friends). They are
//!   re-exported because implementing a port needs them, not as a schema:
//!   for anything kept or sent on, convert an event with
//!   `events::EventDto::from(&event)`.

mod agent;
pub mod approval;
mod config;
mod error;
pub mod events;
pub mod unstable;

pub use agent::{Agent, AgentBuilder, Interrupt, Outcome, Reason, Session};
pub use config::Config;
pub use error::Error;

pub use agent_domain::ports::approval::ApprovalGate;
pub use agent_domain::ports::events::{AgentEvent, EventSink};
/// For implementing [`ApprovalGate`] without adding the dependency yourself.
pub use async_trait::async_trait;
