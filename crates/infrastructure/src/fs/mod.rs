//! Filesystem adapters, all confined to the workspace root.

pub mod context;
pub mod local;
pub(crate) mod resolve;
pub mod scratch;
pub mod search;

pub use context::WorkspaceContextProvider;
pub use local::LocalFileSystem;
pub use scratch::{ScratchDir, ScratchError};
pub use search::IgnoreAwareSearcher;
