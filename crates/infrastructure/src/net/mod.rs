//! Shared outbound-network policy.
//!
//! Anything in this crate that lets model-chosen traffic leave the process -
//! the `web_fetch` tool, the command sandbox's egress proxy - admits its
//! destination through [`guard`]. One implementation of the rules, one
//! allowlist for the operator.

pub(crate) mod guard;
