//! The operator's permission rules: per tool, and optionally per path, host
//! or command, whether a call is let through without asking, always asked
//! about, or refused.
//!
//! The rules sit in front of the ordinary approval policy and can only move a
//! call along it: `deny` refuses what the policy would have allowed, `ask`
//! puts in front of a human what the policy (an `auto` setting, an earlier
//! "approve all") would have let through, and `allow` spares the human a
//! question the policy would have asked. When several rules match one call
//! the strictest wins - **deny > ask > allow** - so adding a rule can never
//! loosen what another rule already tightened.
//!
//! What a rule compares with is the call's *subject*:
//!
//! * file tools: a path, as a [`PathPattern`] (the access policy's language,
//!   with the same Unicode folding). A call that writes is matched on the
//!   path it names **and** on the file it really reaches, symbolic links
//!   resolved - `notes -> secrets/token` is a write to `secrets/`.
//! * `web_fetch`: the URL's host, as a domain suffix (`example.com` also
//!   covers `docs.example.com`).
//! * `run_command`: the start of the command line, word by word. This is a
//!   **fence against mistakes, not a security boundary**: a shell has more
//!   ways to spell a command than a prefix can see (quotes, variables,
//!   `eval`, another program that runs it). Which is why a command can only
//!   be denied or asked about, never allowed.
//!
//! A subject that is there but cannot be read (a URL this module cannot take
//! apart the way the fetcher would) is treated as matching every `deny` and
//! `ask` rule of its kind and no `allow` rule: unknown is never let through
//! on the strength of a rule.

use std::fmt;

use crate::error::DomainError;
use crate::model::access::PathPattern;
use crate::model::tool::ToolName;
use crate::model::workspace::WorkspacePath;

/// What a rule does with the calls it matches. Ordered by strictness, so the
/// strictest of several is their maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

impl Effect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

/// Which kind of subject a rule or a call is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubjectKind {
    Path,
    Domain,
    Command,
}

impl SubjectKind {
    /// The field a rule names it with, for messages.
    pub fn field(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Domain => "domain",
            Self::Command => "command",
        }
    }
}

/// A host and everything below it: `example.com` covers `example.com` and
/// `docs.example.com`, never `badexample.com`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainPattern(String);

impl DomainPattern {
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        let invalid = |reason: &str| Err(DomainError::invalid("domain pattern", reason));
        let domain = raw.trim().trim_start_matches('.').to_ascii_lowercase();
        let domain = domain.trim_end_matches('.');
        if domain.is_empty() {
            return invalid("the domain is empty");
        }
        if domain.contains('*') {
            return invalid(
                "wildcards are not supported: `example.com` already covers every subdomain",
            );
        }
        if domain.contains("://") || domain.contains('/') {
            return invalid("write the host only, e.g. `example.com` rather than a URL");
        }
        if !domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            || domain.contains("..")
        {
            return invalid(
                "only ASCII letters, digits, `-` and `.` are allowed (write an internationalised \
                 name in its `xn--` form)",
            );
        }
        Ok(Self(domain.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `host` must be lowercase without a trailing dot, as [`url_host`]
    /// returns it.
    pub fn covers(&self, host: &str) -> bool {
        host == self.0
            || host
                .strip_suffix(self.0.as_str())
                .is_some_and(|rest| rest.ends_with('.'))
    }
}

/// The first words of a command line: `git push` covers `git push`,
/// `git push origin main` and `cd app && git  push`, never `git pushy`.
///
/// Every simple command in the line is compared - the line is cut at `;`,
/// `&`, `|`, newlines, parentheses and backticks - after skipping leading
/// `NAME=value` assignments. Nothing else is interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPrefix(Vec<String>);

impl CommandPrefix {
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        let words: Vec<String> = raw.split_whitespace().map(str::to_string).collect();
        if words.is_empty() {
            return Err(DomainError::invalid(
                "command pattern",
                "the command is empty",
            ));
        }
        if raw.contains(SEPARATORS) {
            return Err(DomainError::invalid(
                "command pattern",
                "write a single command (`git push`), without `;`, `&`, `|`, parentheses or \
                 backticks - each command in a line is compared on its own",
            ));
        }
        Ok(Self(words))
    }

    pub fn as_str(&self) -> String {
        self.0.join(" ")
    }

    pub fn covers(&self, command_line: &str) -> bool {
        command_line.split(SEPARATORS).any(|simple| {
            let mut words = simple
                .split_whitespace()
                .skip_while(|word| is_assignment(word));
            self.0
                .iter()
                .all(|expected| words.next() == Some(expected.as_str()))
        })
    }
}

/// Where one simple command ends and the next begins, near enough for a
/// fence. `$(` is covered by `(`.
const SEPARATORS: [char; 7] = [';', '&', '|', '\n', '(', ')', '`'];

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// What a rule compares a call with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectPattern {
    Path(PathPattern),
    Domain(DomainPattern),
    Command(CommandPrefix),
}

impl SubjectPattern {
    pub fn kind(&self) -> SubjectKind {
        match self {
            Self::Path(_) => SubjectKind::Path,
            Self::Domain(_) => SubjectKind::Domain,
            Self::Command(_) => SubjectKind::Command,
        }
    }

    fn covers(&self, subject: &Subject) -> bool {
        match (self, subject) {
            (Self::Path(pattern), Subject::Path(path)) => pattern.covers(path),
            (Self::Domain(pattern), Subject::Host(host)) => pattern.covers(host),
            (Self::Command(pattern), Subject::Command(line)) => pattern.covers(line),
            _ => false,
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Path(pattern) => format!("path `{}`", pattern.as_str()),
            Self::Domain(pattern) => format!("domain `{}`", pattern.as_str()),
            Self::Command(pattern) => format!("command `{}`", pattern.as_str()),
        }
    }
}

/// One thing a call is about, as the rules compare it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    Path(WorkspacePath),
    /// Lowercase, without a trailing dot: see [`url_host`].
    Host(String),
    Command(String),
    /// Present in the call, but not in a form that can be compared.
    Unreadable(SubjectKind),
}

impl Subject {
    fn is_unreadable_as(&self, kind: SubjectKind) -> bool {
        matches!(self, Self::Unreadable(unreadable) if *unreadable == kind)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRule {
    pub tool: ToolName,
    /// `None`: every call of the tool.
    pub subject: Option<SubjectPattern>,
    pub effect: Effect,
}

impl PermissionRule {
    /// Whether this rule is about `subjects` (the ones a call of its tool is
    /// about). A rule without a subject is about every call; an unreadable
    /// subject is matched by every rule of its kind (see the module docs for
    /// why `allow` does not rely on this).
    fn covers_any(&self, subjects: &[Subject]) -> bool {
        match &self.subject {
            None => true,
            Some(pattern) => subjects
                .iter()
                .any(|subject| pattern.covers(subject) || subject.is_unreadable_as(pattern.kind())),
        }
    }
}

impl fmt::Display for PermissionRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.effect.as_str(), self.tool)?;
        if let Some(subject) = &self.subject {
            write!(f, " {}", subject.describe())?;
        }
        Ok(())
    }
}

/// What the rules say about one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict<'a> {
    /// Refused, by this rule.
    Deny(&'a PermissionRule),
    /// A human must be asked, whatever the approval policy says, because of
    /// this rule.
    Ask(&'a PermissionRule),
    /// Let through without asking: every subject is covered by an `allow`.
    Allow,
    /// No rule decides; the ordinary approval policy does.
    Unruled,
}

/// Every rule the operator wrote, in the order written (which decides
/// nothing - the strictest match wins - but is the order they are shown in).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionRules {
    rules: Vec<PermissionRule>,
}

impl PermissionRules {
    pub fn new(rules: Vec<PermissionRule>) -> Self {
        Self { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    /// Whether any rule about `tool` compares a subject of `kind` - so a
    /// caller can skip working out subjects nobody will look at.
    pub fn compares(&self, tool: &ToolName, kind: SubjectKind) -> bool {
        self.for_tool(tool)
            .any(|rule| rule.subject.as_ref().is_some_and(|s| s.kind() == kind))
    }

    pub fn mentions(&self, tool: &ToolName) -> bool {
        self.for_tool(tool).next().is_some()
    }

    /// Deny beats ask beats allow. `allow` needs *every* subject covered -
    /// a write whose named path is allowed but whose real target is not
    /// stays a question - and never counts an unreadable one as covered.
    pub fn evaluate(&self, tool: &ToolName, subjects: &[Subject]) -> Verdict<'_> {
        for effect in [Effect::Deny, Effect::Ask] {
            if let Some(rule) = self
                .for_tool(tool)
                .find(|rule| rule.effect == effect && rule.covers_any(subjects))
            {
                return match effect {
                    Effect::Deny => Verdict::Deny(rule),
                    _ => Verdict::Ask(rule),
                };
            }
        }

        let allows: Vec<&PermissionRule> = self
            .for_tool(tool)
            .filter(|rule| rule.effect == Effect::Allow)
            .collect();
        if allows.iter().any(|rule| rule.subject.is_none()) {
            return Verdict::Allow;
        }
        let all_covered = !subjects.is_empty()
            && subjects.iter().all(|subject| {
                allows.iter().any(|rule| {
                    rule.subject
                        .as_ref()
                        .is_some_and(|pattern| pattern.covers(subject))
                })
            });
        if all_covered {
            Verdict::Allow
        } else {
            Verdict::Unruled
        }
    }

    fn for_tool<'a, 'b>(
        &'a self,
        tool: &'b ToolName,
    ) -> impl Iterator<Item = &'a PermissionRule> + use<'a, 'b> {
        self.rules.iter().filter(move |rule| &rule.tool == tool)
    }
}

/// The host of an `http`/`https` URL, lowercase and without a trailing dot -
/// or `None` when the URL is not one this function can take apart exactly as
/// a WHATWG parser would.
///
/// Deliberately narrow, because the fetcher parses with a full URL parser and
/// the two must not disagree about which host a rule is looking at. Anything
/// a full parser would *transform* - percent-escapes, non-ASCII (IDNA),
/// backslashes, credentials, embedded whitespace - comes back `None`, and an
/// unreadable host is never allowed on a rule's strength (see the module
/// docs). An IPv6 literal comes back bracketed, which no domain covers.
pub fn url_host(raw: &str) -> Option<String> {
    let (scheme, rest) = raw.trim().split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.contains(['@', '\\']) || authority.chars().any(char::is_whitespace) {
        return None;
    }
    if authority.starts_with('[') {
        let close = authority.find(']')?;
        return Some(authority[..=close].to_ascii_lowercase());
    }
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        Some(_) => return None,
        None => authority,
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let plain = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    plain.then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ToolName {
        ToolName::new(name).unwrap()
    }

    fn path_rule(name: &str, pattern: &str, effect: Effect) -> PermissionRule {
        PermissionRule {
            tool: tool(name),
            subject: Some(SubjectPattern::Path(PathPattern::parse(pattern).unwrap())),
            effect,
        }
    }

    fn path(raw: &str) -> Subject {
        Subject::Path(WorkspacePath::parse(raw).unwrap())
    }

    #[test]
    fn deny_beats_ask_beats_allow_whatever_the_order_written() {
        let rules = PermissionRules::new(vec![
            path_rule("write_file", "src/", Effect::Allow),
            path_rule("write_file", "src/gen/", Effect::Ask),
            path_rule("write_file", "src/gen/locked/", Effect::Deny),
        ]);
        let write = tool("write_file");

        assert!(matches!(
            rules.evaluate(&write, &[path("src/gen/locked/a.rs")]),
            Verdict::Deny(rule) if rule.effect == Effect::Deny
        ));
        assert!(matches!(
            rules.evaluate(&write, &[path("src/gen/a.rs")]),
            Verdict::Ask(_)
        ));
        assert_eq!(
            rules.evaluate(&write, &[path("src/lib.rs")]),
            Verdict::Allow
        );
        assert_eq!(
            rules.evaluate(&write, &[path("README.md")]),
            Verdict::Unruled
        );
    }

    #[test]
    fn a_rule_is_about_its_own_tool_only() {
        let rules = PermissionRules::new(vec![path_rule("write_file", "src/", Effect::Deny)]);
        assert_eq!(
            rules.evaluate(&tool("edit_file"), &[path("src/lib.rs")]),
            Verdict::Unruled
        );
    }

    #[test]
    fn any_denied_subject_denies_and_every_subject_must_be_allowed() {
        let rules = PermissionRules::new(vec![
            path_rule("write_file", "notes/", Effect::Allow),
            path_rule("write_file", "secrets/", Effect::Deny),
        ]);
        let write = tool("write_file");
        // The named path is allowed; the link it goes through is not.
        let through_a_link = [path("notes/today.md"), path("secrets/token")];
        assert!(matches!(
            rules.evaluate(&write, &through_a_link),
            Verdict::Deny(_)
        ));

        let rules = PermissionRules::new(vec![path_rule("write_file", "notes/", Effect::Allow)]);
        assert_eq!(
            rules.evaluate(&write, &[path("notes/a.md"), path("docs/a.md")]),
            Verdict::Unruled,
            "an allowed name does not carry an unallowed target"
        );
    }

    #[test]
    fn a_rule_without_a_subject_covers_every_call_of_its_tool() {
        let rules = PermissionRules::new(vec![PermissionRule {
            tool: tool("web_search"),
            subject: None,
            effect: Effect::Ask,
        }]);
        assert!(matches!(
            rules.evaluate(&tool("web_search"), &[]),
            Verdict::Ask(_)
        ));
    }

    #[test]
    fn an_unreadable_subject_meets_deny_and_ask_but_never_allow() {
        let domain = |raw: &str, effect| PermissionRule {
            tool: tool("web_fetch"),
            subject: Some(SubjectPattern::Domain(DomainPattern::parse(raw).unwrap())),
            effect,
        };
        let unreadable = [Subject::Unreadable(SubjectKind::Domain)];
        let fetch = tool("web_fetch");

        let denying = PermissionRules::new(vec![domain("evil.example", Effect::Deny)]);
        assert!(matches!(
            denying.evaluate(&fetch, &unreadable),
            Verdict::Deny(_)
        ));
        let allowing = PermissionRules::new(vec![domain("docs.rs", Effect::Allow)]);
        assert_eq!(allowing.evaluate(&fetch, &unreadable), Verdict::Unruled);
        assert_eq!(
            allowing.evaluate(&fetch, &[]),
            Verdict::Unruled,
            "no subject at all is not every subject allowed"
        );
    }

    #[test]
    fn path_rules_fold_names_like_the_access_policy() {
        let rules = PermissionRules::new(vec![path_rule("read_file", "*.key", Effect::Deny)]);
        assert!(matches!(
            rules.evaluate(&tool("read_file"), &[path("certs/server.\u{212A}EY")]),
            Verdict::Deny(_)
        ));
    }

    #[test]
    fn a_domain_covers_itself_and_its_subdomains_only() {
        let docs = DomainPattern::parse("Docs.RS.").unwrap();
        assert!(docs.covers("docs.rs"));
        assert!(docs.covers("api.docs.rs"));
        assert!(!docs.covers("evildocs.rs"));
        assert!(!docs.covers("docs.rs.evil.example"));
    }

    #[test]
    fn domain_patterns_that_would_mean_something_else_are_rejected() {
        for raw in [
            "",
            " . ",
            "*.example.com",
            "https://example.com",
            "a..b",
            "ex ample.com",
        ] {
            assert!(
                DomainPattern::parse(raw).is_err(),
                "`{raw}` must be rejected"
            );
        }
        assert_eq!(
            DomainPattern::parse(".example.com").unwrap().as_str(),
            "example.com"
        );
    }

    #[test]
    fn url_host_reads_only_what_a_full_parser_would_read_the_same_way() {
        assert_eq!(
            url_host("https://Docs.RS/serde").as_deref(),
            Some("docs.rs")
        );
        assert_eq!(
            url_host("http://example.com.:8080?q").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            url_host("HTTPS://example.com#x").as_deref(),
            Some("example.com")
        );
        assert_eq!(url_host("http://[::1]:80/").as_deref(), Some("[::1]"));
        for unreadable in [
            "ftp://example.com/",
            "example.com/path",
            "https://user@example.com/",
            "https://good.example\\@evil.example/",
            "https://exa%6Dple.com/",
            "https://\u{FF45}xample.com/",
            "https://exa\tmple.com/",
            "https://example.com:http/",
            "https:///path",
        ] {
            assert_eq!(url_host(unreadable), None, "`{unreadable}`");
        }
    }

    #[test]
    fn a_command_prefix_matches_whole_words_in_every_simple_command() {
        let push = CommandPrefix::parse("git  push").unwrap();
        assert!(push.covers("git push"));
        assert!(push.covers("  git push origin main"));
        assert!(push.covers("cd app && git push"));
        assert!(push.covers("make test; git push --force"));
        assert!(push.covers("echo $(git push)"));
        assert!(push.covers("GIT_TRACE=1 git push"));
        assert!(!push.covers("git pushy"));
        assert!(!push.covers("git status"));
        assert!(
            !push.covers("echo git push"),
            "only the start of a command counts"
        );
    }

    #[test]
    fn command_patterns_are_single_commands() {
        assert!(CommandPrefix::parse("   ").is_err());
        assert!(CommandPrefix::parse("git push; rm").is_err());
        assert!(CommandPrefix::parse("a | b").is_err());
    }

    #[test]
    fn a_rule_describes_itself_for_the_model() {
        let rule = path_rule("write_file", "src/gen/", Effect::Deny);
        assert_eq!(rule.to_string(), "deny write_file path `src/gen/`");
    }
}
