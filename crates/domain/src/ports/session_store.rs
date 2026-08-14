//! Reading back what a session left behind.
//!
//! The read side of [`ConversationLog`](super::conversation_log::ConversationLog),
//! deliberately kept as a separate port. The loop only ever appends, and
//! handing it a trait that can also delete sessions would be handing it an
//! authority it has no use for.
//!
//! There is one store behind both, not two. A session is resumed by reading
//! the log that was already being written, rather than by keeping a second
//! copy of the history somewhere else - two files that are supposed to say the
//! same thing eventually disagree, usually after a crash, and then there is no
//! way to tell which one is right.

use async_trait::async_trait;

use crate::error::FsError;
use crate::model::conversation::Conversation;

/// Enough to recognise a session in a list without reading it.
///
/// Everything here is answerable from a file's metadata and its opening lines,
/// which is what keeps listing proportional to the number of sessions rather
/// than to how much has been said in them. A message count would not be - it
/// needs every line - and it is the least useful of the three: what someone
/// scanning a list wants is which session was theirs, and which are worth
/// deleting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    /// Opening words of the first thing the user asked, clipped.
    ///
    /// The id is a UUID and says nothing about what the session was for. This
    /// is the part that lets someone pick theirs out of a list. Empty when the
    /// opening request is too long to find near the start of the record.
    pub preview: String,
    pub bytes: u64,
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Every stored session, **newest first**.
    ///
    /// The ordering is the implementation's job rather than the caller's,
    /// because what "newest" means is a property of the storage - and pushing
    /// it into the domain would mean giving the domain a notion of time it
    /// otherwise does not need.
    async fn list(&self) -> Result<Vec<SessionSummary>, FsError>;

    /// Rebuilds the conversation of `session_id`.
    ///
    /// A record that has been truncated - a crash mid-write, a partial copy -
    /// yields everything up to the damage rather than an error. A history that
    /// stops early is still a usable history; refusing to open it at all is
    /// the one outcome that helps nobody.
    async fn load(&self, session_id: &str) -> Result<Conversation, FsError>;

    /// Id of the most recently written session, if there is one.
    async fn latest(&self) -> Result<Option<String>, FsError>;

    async fn delete(&self, session_id: &str) -> Result<(), FsError>;
}
