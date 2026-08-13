//! Ambient-context port.

use async_trait::async_trait;

use crate::error::FsError;
use crate::model::context::ContextSnapshot;

/// Collects everything the model should know about its environment before the
/// conversation starts.
#[async_trait]
pub trait ContextProvider: Send + Sync {
    async fn snapshot(&self) -> Result<ContextSnapshot, FsError>;
}
