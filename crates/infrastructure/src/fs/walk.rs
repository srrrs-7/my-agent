//! The one way this crate walks the workspace for the model.
//!
//! Search, glob and the context snapshot all show the model what is in the
//! workspace, so they agree on what counts: hidden entries and whatever
//! `.gitignore` or `.git/info/exclude` excludes are left out - without that,
//! the first search in any Rust project would drown the model in `target/` -
//! and no symlink is followed, so a link out of the workspace is never
//! walked into.
//!
//! `tools/dependency_watch.rs` deliberately walks differently and does not
//! use this: it is a safety check rather than a view for the model, so it
//! walks hidden directories too and ignores the user's global excludes.

use std::path::Path;

use ignore::WalkBuilder;

/// A walker over `base` with the settings above. Callers adjust it further
/// (depth, hidden entries for a pattern that names one) before building it.
pub(crate) fn workspace_walker(base: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(base);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        // Apply ignore rules even when the workspace is not a git checkout.
        .require_git(false)
        .follow_links(false);
    builder
}
