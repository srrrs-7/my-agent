//! A path pattern the model asks to match files against.
//!
//! The lexical rules live here, like [`WorkspacePath`]'s, so that a
//! [`GlobQuery`] cannot carry a pattern that reaches outside the subtree it is
//! matched in. Whether the pattern is valid glob *syntax* is the searcher's
//! question: that depends on the matcher an adapter uses.
//!
//! [`WorkspacePath`]: crate::model::workspace::WorkspacePath
//! [`GlobQuery`]: crate::ports::search::GlobQuery

use std::fmt;

use crate::error::DomainError;

/// A glob in `.gitignore` syntax, relative to the directory it is matched in:
/// never empty, never absolute, never containing `..` or a NUL byte.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GlobPattern(String);

impl GlobPattern {
    /// Trims `raw` and refuses a pattern that reaches outside the subtree.
    ///
    /// Such a pattern could never match anything the walk visits, so
    /// accepting it would answer "no files" - which reads as "nothing like
    /// that exists" rather than "you asked the wrong way". Each refusal says
    /// what to pass instead, because the model is the one that reads it.
    pub fn new(raw: &str) -> Result<Self, DomainError> {
        let refuse = |reason: &str| Err(DomainError::invalid("glob", reason));
        let pattern = raw.trim();
        if pattern.is_empty() {
            return refuse("the glob is empty. Pass a pattern such as `**/*.rs`");
        }
        if pattern.contains('\0') {
            return refuse("the glob contains a NUL byte");
        }
        let bytes = pattern.as_bytes();
        let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        if pattern.starts_with(['/', '\\']) || drive {
            return refuse(
                "the glob must be relative to `path`, not absolute. Drop the leading `/` and \
                 pass the directory to search as `path` instead",
            );
        }
        if pattern.split(['/', '\\']).any(|segment| segment == "..") {
            return refuse(
                "the glob must not contain `..`: it only matches inside the workspace. Pass the \
                 directory to search as `path` instead",
            );
        }
        Ok(Self(pattern.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GlobPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_refuses_patterns_that_reach_outside_the_workspace() {
        for pattern in [
            "../*.rs",
            "src/../../etc/*",
            "/etc/passwd",
            "\\\\server\\share",
            "C:/x",
            "  ",
        ] {
            let error = GlobPattern::new(pattern).unwrap_err();
            assert!(
                matches!(error, DomainError::Invalid { kind: "glob", .. }),
                "`{pattern}` must be refused, got {error:?}"
            );
        }
    }

    #[test]
    fn a_pattern_is_kept_trimmed() {
        assert_eq!(
            GlobPattern::new("  src/**/*.rs\n").unwrap().as_str(),
            "src/**/*.rs"
        );
    }

    #[test]
    fn a_refusal_tells_the_model_what_to_pass_instead() {
        let error = GlobPattern::new("/etc/*").unwrap_err().to_string();
        assert!(
            error.contains("pass the directory to search as `path`"),
            "{error}"
        );
    }
}
