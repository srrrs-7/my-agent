//! Which files inside the workspace the file tools may not touch.
//!
//! The workspace boundary keeps the model *in*; this keeps it *off* the few
//! things inside that are not project files: credentials (`.env`, private
//! keys), the conversation record the agent writes about itself, and git's own
//! metadata. `.gitignore` does not help here. It hides a file from search, but
//! an ignored file is still one `read_file` away - and `.env` is ignored in
//! nearly every repository precisely because it holds secrets.
//!
//! Like the workspace boundary this is a rule of the agent, so it lives in the
//! domain. Matching is purely lexical. Adapters apply it twice, to the path the
//! model asked for and again to the symlink-resolved path, because a link can
//! give a protected file an innocent name (`notes -> .env`) and an innocent
//! file a protected one (`.env -> config/app.env`).
//!
//! The pattern language is deliberately small, so that what a rule covers can
//! be read off the rule:
//!
//! * a name with at most one `*` (`.env`, `.env.*`, `*.pem`) matches a file or
//!   directory of that name at any depth - everything below a matching
//!   directory included;
//! * a path with a `/` before its end (`.agent/sessions/`) is a directory
//!   relative to the workspace root, and matches it and everything below it.
//!
//! Matching ignores ASCII case. Case-insensitive filesystems (macOS, Windows)
//! open `.ENV` as `.env`, and refusing `.ENV` on a case-sensitive one costs
//! nothing.
//!
//! There is no way to *remove* a rule. [`AccessPolicy::standard`] is the only
//! starting point and every other constructor adds to it, so a configuration
//! can make the policy stricter and never weaker.

use std::path::Component;

use crate::error::DomainError;
use crate::model::workspace::WorkspacePath;

/// What a file operation is about to do with a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileAccess {
    Read,
    Write,
}

impl FileAccess {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// Names that are never project files and are refused in both directions.
const READ_DENIED: [&str; 9] = [
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "id_rsa*",
    "id_ed25519*",
    ".netrc",
    ".npmrc",
    ".pypirc",
];

/// Template files that match a rule above but hold no secrets by convention.
/// Built in rather than configurable: an exception an operator could write
/// would also be a way to remove a rule.
const READ_EXCEPTIONS: [(&str, &[&str]); 1] = [(".env.*", &[".env.example"])];

/// Readable, but never written. Rewriting `.git/config` (`core.fsmonitor`,
/// `core.hooksPath`) or a hook runs code the next time anyone uses git, and a
/// file named `.git` redirects git to another directory altogether.
const WRITE_DENIED: [&str; 1] = [".git/"];

/// One path component as a pattern: a literal, or a literal with one `*`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Exact(String),
    Wildcard { prefix: String, suffix: String },
}

impl Segment {
    fn glob(raw: &str) -> Self {
        let raw = raw.to_ascii_lowercase();
        match raw.split_once('*') {
            Some((prefix, suffix)) => Self::Wildcard {
                prefix: prefix.to_string(),
                suffix: suffix.to_string(),
            },
            None => Self::Exact(raw),
        }
    }

    /// For a name that came from the filesystem rather than from a pattern:
    /// a `*` in it is a character, not a wildcard.
    fn literal(raw: &str) -> Self {
        Self::Exact(raw.to_ascii_lowercase())
    }

    /// `component` must already be lowercased.
    fn matches(&self, component: &str) -> bool {
        match self {
            Self::Exact(name) => component == name,
            Self::Wildcard { prefix, suffix } => {
                component.len() >= prefix.len() + suffix.len()
                    && component.starts_with(prefix.as_str())
                    && component.ends_with(suffix.as_str())
            }
        }
    }
}

/// One rule of an [`AccessPolicy`]. See the module documentation for the
/// syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPattern {
    /// As written, for `agent doctor`.
    raw: String,
    /// `true`: `segments` name a directory under the root, compared from the
    /// first component. `false`: a single segment compared with every
    /// component.
    anchored: bool,
    segments: Vec<Segment>,
    /// Lowercased names the rule leaves alone although they match it.
    exceptions: Vec<String>,
}

impl PathPattern {
    /// Parses an operator-written pattern.
    ///
    /// Anything the matcher would read differently from how it looks is an
    /// error rather than a guess: `?` and `[...]` would otherwise match only
    /// themselves, which is a rule that silently protects nothing.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        let raw = raw.trim();
        let invalid = |reason: &str| Err(DomainError::invalid("access pattern", reason));

        if raw.is_empty() {
            return invalid("the pattern is empty");
        }
        if raw.starts_with('!') {
            return invalid(
                "exceptions (`!`) are not supported: the built-in rules can be extended, \
                 not narrowed",
            );
        }
        if raw.contains('\0') {
            return invalid("the pattern contains a NUL byte");
        }
        if raw.contains(['?', '[', ']', '{', '}']) {
            return invalid("only `*` is supported as a wildcard");
        }
        if raw.matches('*').count() > 1 {
            return invalid("at most one `*` is supported (no `**`)");
        }
        if raw.starts_with('/') {
            return invalid(
                "the pattern must be relative to the workspace root, e.g. `secrets/` or \
                 `config/master.key`",
            );
        }

        let mut segments = Vec::new();
        for part in raw.split('/') {
            match part {
                "" | "." => {}
                ".." => return invalid("`..` is not allowed in a pattern"),
                _ => segments.push(Segment::glob(part)),
            }
        }
        if segments.is_empty() {
            return invalid("the pattern names nothing");
        }

        Ok(Self {
            raw: raw.to_string(),
            anchored: segments.len() > 1,
            segments,
            exceptions: Vec::new(),
        })
    }

    /// The directory `path` and everything below it, taken literally. For
    /// locations the program computes, such as the session log directory.
    pub fn directory(path: &WorkspacePath) -> Result<Self, DomainError> {
        if path.is_root() {
            return Err(DomainError::invalid(
                "access pattern",
                "the workspace root itself cannot be protected - every file would be refused",
            ));
        }
        Ok(Self {
            raw: format!("{}/", path.display()),
            anchored: true,
            segments: components(path)
                .iter()
                .map(|name| Segment::literal(name))
                .collect(),
            exceptions: Vec::new(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// `components` must already be lowercased.
    fn matches(&self, components: &[String]) -> bool {
        if self.anchored {
            return components.len() >= self.segments.len()
                && self
                    .segments
                    .iter()
                    .zip(components)
                    .all(|(segment, component)| segment.matches(component));
        }
        let segment = &self.segments[0];
        components.iter().any(|component| {
            segment.matches(component) && !self.exceptions.iter().any(|name| name == component)
        })
    }

    fn describe(&self) -> String {
        if self.exceptions.is_empty() {
            self.raw.clone()
        } else {
            format!("{} (except {})", self.raw, self.exceptions.join(", "))
        }
    }
}

/// The paths inside the workspace that file operations refuse.
///
/// Writes are refused for everything reads are, plus the write-only rules: a
/// file the model may not see is not one it may overwrite either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessPolicy {
    read_denied: Vec<PathPattern>,
    write_denied: Vec<PathPattern>,
}

impl AccessPolicy {
    /// The built-in rules. Every policy starts here.
    pub fn standard() -> Self {
        let builtin = |raw: &str| PathPattern::parse(raw).expect("built-in pattern is valid");
        let read_denied = READ_DENIED
            .iter()
            .map(|raw| {
                let mut pattern = builtin(raw);
                if let Some((_, names)) = READ_EXCEPTIONS.iter().find(|(rule, _)| rule == raw) {
                    pattern.exceptions = names.iter().map(|name| name.to_string()).collect();
                }
                pattern
            })
            .collect();
        Self {
            read_denied,
            write_denied: WRITE_DENIED.iter().map(|raw| builtin(raw)).collect(),
        }
    }

    /// Refuses reading (and therefore writing) whatever `pattern` matches.
    #[must_use]
    pub fn with_read_denied(mut self, pattern: PathPattern) -> Self {
        if !self.read_denied.contains(&pattern) {
            self.read_denied.push(pattern);
        }
        self
    }

    /// Refuses writing whatever `pattern` matches, leaving it readable.
    #[must_use]
    pub fn with_write_denied(mut self, pattern: PathPattern) -> Self {
        if !self.write_denied.contains(&pattern) {
            self.write_denied.push(pattern);
        }
        self
    }

    pub fn permits(&self, path: &WorkspacePath, access: FileAccess) -> bool {
        let components: Vec<String> = components(path)
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect();
        let write_only: &[PathPattern] = match access {
            FileAccess::Read => &[],
            FileAccess::Write => &self.write_denied,
        };
        !self
            .read_denied
            .iter()
            .chain(write_only)
            .any(|pattern| pattern.matches(&components))
    }

    /// One line for `agent doctor`.
    pub fn describe(&self) -> String {
        let list = |patterns: &[PathPattern]| {
            patterns
                .iter()
                .map(PathPattern::describe)
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "unreadable: {}; also unwritable: {}",
            list(&self.read_denied),
            list(&self.write_denied)
        )
    }
}

fn components(path: &WorkspacePath) -> Vec<String> {
    path.as_path()
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(raw: &str) -> WorkspacePath {
        WorkspacePath::parse(raw).unwrap()
    }

    fn readable(policy: &AccessPolicy, raw: &str) -> bool {
        policy.permits(&path(raw), FileAccess::Read)
    }

    fn writable(policy: &AccessPolicy, raw: &str) -> bool {
        policy.permits(&path(raw), FileAccess::Write)
    }

    #[test]
    fn env_files_are_refused_at_any_depth_but_the_example_is_not() {
        let policy = AccessPolicy::standard();
        for denied in [
            ".env",
            ".env.local",
            "sub/dir/.env",
            "apps/web/.env.production",
        ] {
            assert!(!readable(&policy, denied), "`{denied}` must be unreadable");
            assert!(!writable(&policy, denied), "`{denied}` must be unwritable");
        }
        assert!(readable(&policy, ".env.example"));
        assert!(readable(&policy, "sub/.env.example"));
        assert!(
            readable(&policy, "src/env.rs") && readable(&policy, "docs/.envrc.md"),
            "only the names themselves are protected"
        );
    }

    #[test]
    fn every_built_in_secret_name_is_refused() {
        let policy = AccessPolicy::standard();
        for denied in [
            "x.pem",
            "certs/server.key",
            "id_rsa",
            "home/.ssh/id_rsa.pub",
            "id_ed25519",
            ".netrc",
            ".npmrc",
            "pkg/.pypirc",
        ] {
            assert!(!readable(&policy, denied), "`{denied}` must be unreadable");
        }
        assert!(readable(&policy, "src/keys.rs"));
        assert!(readable(&policy, "README.md"));
    }

    #[test]
    fn everything_below_a_matching_directory_is_refused() {
        let policy = AccessPolicy::standard();
        assert!(!readable(&policy, ".env.d/database"));
        assert!(!readable(&policy, "secrets.key/inner.txt"));
    }

    #[test]
    fn git_metadata_is_readable_but_never_written() {
        let policy = AccessPolicy::standard();
        assert!(readable(&policy, ".git/config"));
        assert!(!writable(&policy, ".git/config"));
        assert!(!writable(&policy, ".git/hooks/pre-commit"));
        assert!(
            !writable(&policy, "vendor/lib/.git"),
            "a file named `.git` points git at another directory"
        );
        assert!(writable(&policy, ".gitignore"));
        assert!(writable(&policy, ".github/workflows/ci.yml"));
    }

    #[test]
    fn matching_ignores_ascii_case() {
        let policy = AccessPolicy::standard();
        assert!(!readable(&policy, ".ENV"));
        assert!(!readable(&policy, "Server.PEM"));
        assert!(!writable(&policy, ".Git/config"));
        assert!(readable(&policy, ".ENV.EXAMPLE"));
    }

    #[test]
    fn the_session_directory_is_refused_by_its_location_only() {
        let policy = AccessPolicy::standard()
            .with_read_denied(PathPattern::directory(&path(".agent/sessions")).unwrap());

        assert!(!readable(&policy, ".agent/sessions/a.jsonl"));
        assert!(!writable(&policy, ".agent/sessions/a.jsonl"));
        assert!(!readable(&policy, ".agent/sessions"));
        assert!(
            readable(&policy, ".agent/instructions.md"),
            "the rest of `.agent/` is project configuration"
        );
        assert!(
            readable(&policy, "docs/.agent/sessions/a.jsonl"),
            "a location is anchored at the root"
        );
    }

    #[test]
    fn added_patterns_extend_the_built_in_list() {
        let policy = AccessPolicy::standard()
            .with_read_denied(PathPattern::parse("*.p12").unwrap())
            .with_read_denied(PathPattern::parse("config/master.key").unwrap())
            .with_read_denied(PathPattern::parse("secrets/").unwrap())
            .with_write_denied(PathPattern::parse("Cargo.lock").unwrap());

        assert!(!readable(&policy, "certs/client.p12"));
        assert!(!readable(&policy, "config/master.key"));
        assert!(!readable(&policy, "secrets/prod.yml"));
        assert!(
            !readable(&policy, "deploy/secrets/prod.yml"),
            "a name without an inner `/` matches at any depth"
        );
        assert!(readable(&policy, "Cargo.lock"));
        assert!(!writable(&policy, "Cargo.lock"));
        assert!(
            !readable(&policy, ".env"),
            "the built-in rules stay in force"
        );
    }

    #[test]
    fn a_pattern_with_an_inner_slash_is_anchored_at_the_root() {
        let policy =
            AccessPolicy::standard().with_read_denied(PathPattern::parse("config/*.yml").unwrap());
        assert!(!readable(&policy, "config/prod.yml"));
        assert!(!readable(&policy, "config/prod.yml/extra"));
        assert!(readable(&policy, "app/config/prod.yml"));
        assert!(readable(&policy, "config"));
    }

    #[test]
    fn the_root_itself_is_never_matched() {
        let policy = AccessPolicy::standard();
        assert!(policy.permits(&WorkspacePath::root(), FileAccess::Read));
        assert!(policy.permits(&WorkspacePath::root(), FileAccess::Write));
    }

    #[test]
    fn patterns_that_would_mean_something_else_are_rejected() {
        for raw in [
            "",
            "   ",
            "!.env.local",
            "/etc/shadow",
            "../outside",
            "a/../../b",
            "**/*.pem",
            "*.*",
            "secret?.txt",
            "[ab].key",
            "{a,b}.key",
            "./",
        ] {
            assert!(
                PathPattern::parse(raw).is_err(),
                "`{raw}` must be rejected rather than read some other way"
            );
        }
    }

    #[test]
    fn harmless_spelling_variants_are_accepted() {
        assert_eq!(PathPattern::parse(" .env ").unwrap().as_str(), ".env");
        let dotted = PathPattern::parse("./config//secret.yml").unwrap();
        let policy = AccessPolicy::standard().with_read_denied(dotted);
        assert!(!readable(&policy, "config/secret.yml"));
    }

    #[test]
    fn the_workspace_root_cannot_become_a_protected_directory() {
        assert!(PathPattern::directory(&WorkspacePath::root()).is_err());
    }

    #[test]
    fn a_directory_name_is_taken_literally() {
        let policy = AccessPolicy::standard()
            .with_read_denied(PathPattern::directory(&path("logs/*")).unwrap());
        assert!(!readable(&policy, "logs/*/a.jsonl"));
        assert!(readable(&policy, "logs/today/a.jsonl"));
    }

    #[test]
    fn adding_a_pattern_twice_lists_it_once() {
        let policy = AccessPolicy::standard()
            .with_read_denied(PathPattern::parse(".env").unwrap())
            .with_write_denied(PathPattern::parse(".git/").unwrap());
        assert_eq!(policy, AccessPolicy::standard(), "{}", policy.describe());
    }

    #[test]
    fn describe_lists_every_rule_on_one_line() {
        let described = AccessPolicy::standard()
            .with_read_denied(PathPattern::directory(&path(".agent/sessions")).unwrap())
            .describe();
        assert!(!described.contains('\n'), "{described}");
        for shown in [
            ".env,",
            ".env.* (except .env.example)",
            "*.pem",
            "id_ed25519*",
            ".agent/sessions/",
            "also unwritable: .git/",
        ] {
            assert!(described.contains(shown), "`{shown}` missing: {described}");
        }
    }
}
