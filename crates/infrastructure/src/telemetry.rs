//! Tracing setup.
//!
//! Logs go to stderr so that stdout stays a clean channel for the agent's
//! answer - which means `agent run "..." > answer.md` does the obvious thing.

use tracing_subscriber::EnvFilter;

pub fn init(default_filter: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .without_time()
        .try_init();
}
