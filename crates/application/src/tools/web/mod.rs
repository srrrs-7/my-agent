//! Web tools.
//!
//! Unlike the file tools these cross the process boundary outward, so their
//! safety class is [`agent_domain::model::tool::ToolSafety::Network`]: the
//! default approval policy confirms every call, and the human sees the full
//! URL - including whatever the model put in its query string - before
//! anything is sent.

mod fetch;

pub use fetch::WebFetchTool;
