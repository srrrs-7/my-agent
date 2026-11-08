//! Outbound web access, guarded.
//!
//! [`GuardedWebFetcher`] implements the [`agent_domain::ports::web::WebFetcher`]
//! port; [`guard`] owns the admission policy (private-address and internal-host
//! blocking), [`html`] the text reduction. [`BraveSearcher`] implements
//! [`agent_domain::ports::web::WebSearcher`] against one fixed search API.

mod fetcher;
mod html;
mod search;

pub use fetcher::{GuardedWebFetcher, WebFetchConfig};
pub use search::{BraveSearcher, SearchApiKey};
