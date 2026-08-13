//! Filesystem adapters, all confined to the workspace root.

pub mod context;
pub mod local;
pub mod search;

pub use context::WorkspaceContextProvider;
pub use local::LocalFileSystem;
pub use search::IgnoreAwareSearcher;
