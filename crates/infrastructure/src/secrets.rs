//! What counts as a secret in this process, and hiding the ones it holds.
//!
//! Two consumers share these rules so that they cannot drift apart: the
//! command sandbox, which keeps secrets out of a child's environment
//! (`exec::env`), and the [`Redactor`], which replaces them in whatever a tool
//! returns before it reaches the model, the session log, or a notification
//! excerpt. A value too sensitive to hand to a command is too sensitive to
//! show the model when a command prints it anyway.
//!
//! Secrets are known in two ways:
//!
//! * **configured** - provider API keys, the webhook URL. The operator said
//!   so, and they are taken as they are ([`MIN_SECRET_CHARS`]).
//! * **guessed from a name** ([`is_secret`], [`secret_values`]) - the
//!   conventions nearly every tool follows. A name is only a guess, so the
//!   value has to look like a secret as well ([`is_distinctive`]).
//!
//! Only exact occurrences are redacted. An encoded, split or truncated secret
//! passes through, and so does any secret this process was never given. This
//! narrows the leak; it does not close it.

use std::borrow::Cow;
use std::fmt;

/// Variables removed from every child environment.
///
/// Matching is on the *name*, case-insensitively, because a value cannot be
/// told apart from ordinary text. The suffix list catches the conventions
/// nearly every tool follows; the exact names catch this agent's own
/// configuration, which would otherwise let a command discover the endpoint
/// and credentials it is running under.
const SECRET_SUFFIXES: [&str; 6] = [
    "_API_KEY",
    "_TOKEN",
    "_SECRET",
    "_SECRET_KEY",
    "_PASSWORD",
    "_CREDENTIALS",
];

const SECRET_EXACT: [&str; 8] = [
    "AGENT_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "GITHUB_TOKEN",
    // Not secrets by name, but by effect: each points at an agent socket that
    // will sign with the user's keys on request. The sandbox does not stop a
    // child from connecting to a unix socket, so removing the address is what
    // takes the capability away.
    "SSH_AUTH_SOCK",
    "GPG_AGENT_INFO",
];

/// Names whose value says *where* a capability is rather than being one: the
/// address of an agent socket. Removed from children for what they grant, but
/// the path itself is no secret, and redacting it would only mangle every
/// mention of a socket directory in tool output.
const LOCATION_SUFFIXES: [&str; 2] = ["_SOCK", "_INFO"];

/// What a secret is replaced with.
pub const REDACTED: &str = "[REDACTED]";

/// Values shorter than this are not treated as secrets: they would also
/// match ordinary text, and a redactor that mangles every third word gets
/// turned off.
///
/// This is the floor for secrets the operator configured. Values merely
/// *guessed* to be secrets from a variable's name face a stricter test before
/// they get here ([`secret_values`]), because a guess is often an ordinary
/// word.
pub const MIN_SECRET_CHARS: usize = 8;

/// A value found under a secret-looking name is redacted only from this
/// length on - longer than [`MIN_SECRET_CHARS`], which applies to the secrets
/// the operator configured.
pub(crate) const MIN_INFERRED_SECRET_CHARS: usize = 12;

/// True when a variable must not reach a child process.
pub(crate) fn is_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_EXACT.contains(&upper.as_str())
        || SECRET_SUFFIXES.iter().any(|suffix| upper.ends_with(suffix))
}

/// The values of the variables in `vars` that [`is_secret`] keeps from a
/// child process, and that look like secrets themselves.
///
/// The same names decide what is redacted from tool output, so that the two
/// lists cannot drift: a value too sensitive to hand to a command is too
/// sensitive to show the model when a command prints it anyway.
///
/// The value has to pass [`is_distinctive`] as well. A name is only a guess:
/// `POSTGRES_PASSWORD=postgres` is a dev-container default, and redacting
/// `postgres` would turn every mention of the word in a file the model reads
/// into `[REDACTED]` - which the model then writes back, corrupting the file.
/// The operator's own secrets are not guessed and skip this test.
pub(crate) fn secret_values(
    vars: impl IntoIterator<Item = (String, String)>,
) -> impl Iterator<Item = String> {
    vars.into_iter()
        .filter(|(name, value)| is_secret(name) && !is_location(name) && is_distinctive(value))
        .map(|(_, value)| value)
}

fn is_location(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    LOCATION_SUFFIXES
        .iter()
        .any(|suffix| upper.ends_with(suffix))
}

/// Whether a value is unlike ordinary text: at least
/// [`MIN_INFERRED_SECRET_CHARS`] long and mixing at least two of lower case,
/// upper case, digits and symbols. Generated keys and tokens nearly always
/// pass; dictionary words, which collide with prose and code, never do.
pub(crate) fn is_distinctive(value: &str) -> bool {
    if value.chars().count() < MIN_INFERRED_SECRET_CHARS {
        return false;
    }
    let classes = [
        value.chars().any(|c| c.is_lowercase()),
        value.chars().any(|c| c.is_uppercase()),
        value.chars().any(|c| c.is_ascii_digit()),
        value
            .chars()
            .any(|c| !c.is_alphanumeric() && !c.is_whitespace()),
    ];
    classes.into_iter().filter(|&present| present).count() >= 2
}

/// A set of known secret values and the replacement of them in text.
///
/// `Debug` shows how many there are, never what they are.
#[derive(Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut secrets: Vec<String> = secrets
            .into_iter()
            .filter(|secret| secret.chars().count() >= MIN_SECRET_CHARS)
            .collect();
        secrets.sort();
        secrets.dedup();
        Self { secrets }
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// `text` with every occurrence of every secret replaced by [`REDACTED`].
    ///
    /// All matches are found before anything is replaced, and overlapping
    /// ones merged, so two secrets sharing characters are both hidden whole -
    /// replacing them one after the other would leave the tail of the second.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut found: Vec<(usize, usize)> = self
            .secrets
            .iter()
            .flat_map(|secret| {
                text.match_indices(secret.as_str())
                    .map(|(start, matched)| (start, start + matched.len()))
            })
            .collect();
        if found.is_empty() {
            return Cow::Borrowed(text);
        }
        found.sort_unstable();

        // Overlapping or touching matches become one span.
        let mut spans: Vec<(usize, usize)> = Vec::with_capacity(found.len());
        for (start, end) in found {
            match spans.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => spans.push((start, end)),
            }
        }

        // Match boundaries are character boundaries: a valid UTF-8 needle
        // found in valid UTF-8 starts and ends on one.
        let mut redacted = String::with_capacity(text.len());
        let mut copied = 0;
        for (start, end) in spans {
            redacted.push_str(&text[copied..start]);
            redacted.push_str(REDACTED);
            copied = end;
        }
        redacted.push_str(&text[copied..]);
        Cow::Owned(redacted)
    }
}

impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Redactor({} secrets)", self.secrets.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conventional_secret_names_are_caught() {
        for name in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "AGENT_API_KEY",
            "AGENT_PROVIDER_CLOUD_API_KEY",
            "GITHUB_TOKEN",
            "NPM_TOKEN",
            "MY_APP_SECRET",
            "DB_PASSWORD",
            "GCP_CREDENTIALS",
            "AWS_SECRET_ACCESS_KEY",
            "SSH_AUTH_SOCK",
            "GPG_AGENT_INFO",
        ] {
            assert!(is_secret(name), "{name} must be scrubbed");
        }
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(is_secret("openai_api_key"));
        assert!(is_secret("Github_Token"));
    }

    #[test]
    fn ordinary_variables_survive() {
        for name in [
            "PATH",
            "HOME",
            "CARGO_TARGET_DIR",
            "RUST_LOG",
            "AGENT_WORKSPACE",
            "AGENT_MAX_ITERATIONS",
            // A name that merely mentions a token without being one.
            "TOKENIZER_PARALLELISM",
        ] {
            assert!(!is_secret(name), "{name} must be kept");
        }
    }

    #[test]
    fn a_dictionary_word_under_a_secret_name_is_not_redacted() {
        let values: Vec<String> = secret_values(
            [
                ("POSTGRES_PASSWORD", "postgres"),
                ("MYSQL_ROOT_PASSWORD", "averylongpassword"),
                ("OPENAI_API_KEY", "sk-proj-AbC123dEf456gHi789"),
                ("GITHUB_TOKEN", "ghp_0123456789abcdef"),
            ]
            .map(|(name, value)| (name.to_string(), value.to_string())),
        )
        .collect();
        assert_eq!(
            values,
            ["sk-proj-AbC123dEf456gHi789", "ghp_0123456789abcdef"],
            "only values unlike ordinary text are redacted"
        );
    }

    #[test]
    fn socket_locations_are_withheld_from_children_but_not_redacted() {
        assert!(is_secret("SSH_AUTH_SOCK"));
        let values: Vec<String> = secret_values(
            [
                ("SSH_AUTH_SOCK", "/tmp/ssh-XXXXabcd1234/agent.42"),
                ("GPG_AGENT_INFO", "/run/user/1000/gnupg/S.gpg-agent:0:1"),
            ]
            .map(|(name, value)| (name.to_string(), value.to_string())),
        )
        .collect();
        assert!(values.is_empty(), "{values:?}");
    }

    #[test]
    fn distinctive_values_mix_classes_and_are_long_enough() {
        assert!(is_distinctive("sk-proj-AbC123"));
        assert!(is_distinctive("correct-horse-battery"));
        assert!(!is_distinctive("Ab1-short"), "too short");
        assert!(!is_distinctive("onlylowercaseletters"), "one class");
        assert!(!is_distinctive("012345678901234"), "one class");
    }

    #[test]
    fn a_provider_alias_key_is_a_secret_but_the_alias_list_is_not() {
        // Both start with AGENT_PROVIDER; only one carries a credential.
        assert!(is_secret("AGENT_PROVIDER_CLOUD_API_KEY"));
        assert!(!is_secret("AGENT_PROVIDERS"));
    }

    const KEY: &str = "sk-ant-api03-0123456789abcdef";

    #[test]
    fn every_occurrence_is_replaced_and_the_rest_is_kept() {
        let redactor = Redactor::new([KEY.to_string()]);
        assert_eq!(
            redactor.redact(&format!("a {KEY} b {KEY}{KEY} c")),
            "a [REDACTED] b [REDACTED] c"
        );
    }

    #[test]
    fn text_without_a_secret_is_returned_as_is() {
        let redactor = Redactor::new([KEY.to_string()]);
        assert!(matches!(
            redactor.redact("nothing to hide"),
            Cow::Borrowed("nothing to hide")
        ));
        assert!(matches!(
            Redactor::default().redact("anything"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn short_values_are_not_treated_as_secrets() {
        let redactor = Redactor::new(["topic".to_string(), "1234567".to_string()]);
        assert!(redactor.is_empty());
        assert_eq!(redactor.redact("topic 1234567"), "topic 1234567");
    }

    #[test]
    fn overlapping_secrets_are_hidden_whole() {
        let redactor = Redactor::new([
            "abcdefgh-1234".to_string(),
            "1234-ijklmnop".to_string(),
            "cdefgh-1".to_string(),
        ]);
        assert_eq!(
            redactor.redact("x abcdefgh-1234-ijklmnop y"),
            "x [REDACTED] y"
        );
    }

    #[test]
    fn non_ascii_text_around_a_secret_survives() {
        let redactor = Redactor::new([KEY.to_string()]);
        assert_eq!(
            redactor.redact(&format!("鍵は{KEY}です")),
            "鍵は[REDACTED]です"
        );
    }

    #[test]
    fn debug_output_does_not_show_the_secrets() {
        let shown = format!("{:?}", Redactor::new([KEY.to_string()]));
        assert_eq!(shown, "Redactor(1 secrets)");
    }
}
