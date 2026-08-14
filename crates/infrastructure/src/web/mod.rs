//! Outbound web access, guarded.
//!
//! [`GuardedWebFetcher`] implements the [`agent_domain::ports::web::WebFetcher`]
//! port; [`guard`] owns the admission policy (private-address and internal-host
//! blocking), [`html`] the text reduction.

mod fetcher;
mod html;

pub use fetcher::{GuardedWebFetcher, WebFetchConfig};
