//! Environment scrubbing for child processes.
//!
//! The cheapest exfiltration route out of a sandboxed command is the
//! environment it inherits: an API key in `AGENT_API_KEY` needs no filesystem
//! access and no clever trick, just `echo $AGENT_API_KEY`. Scrubbing costs
//! nothing, works identically on every platform, and does not depend on any
//! sandbox being available - so it runs even in `SandboxKind::None`.
//!
//! Two rules decide what goes, and neither is a list of this agent's own
//! variable names to keep in step with the settings:
//!
//! * **By name** ([`is_secret`]): the conventions nearly every tool follows.
//! * **By value**: any variable holding one of the secrets the operator
//!   configured - provider keys, the webhook URL. `AGENT_NOTIFY_WEBHOOK_URL`
//!   carries no secret-looking suffix, yet whoever can read it can post to the
//!   user's phone; matching on what the settings hold catches it, and every
//!   future setting of the kind, without naming it.
use std::ffi::OsString;

use crate::tools::redact::MIN_SECRET_CHARS;

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

/// Removes every secret variable from a child's environment: by name, and by
/// holding one of `withheld` (the operator's configured secrets).
pub(crate) fn scrub(command: &mut tokio::process::Command, withheld: &[String]) {
    for name in withheld_names(std::env::vars_os(), withheld) {
        command.env_remove(name);
    }
}

/// The names in `vars` that [`scrub`] removes.
fn withheld_names(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    withheld: &[String],
) -> Vec<OsString> {
    // Short parts (a one-letter URL path) would match unrelated values; the
    // redactor ignores them for the same reason.
    let withheld: Vec<&str> = withheld
        .iter()
        .map(String::as_str)
        .filter(|secret| secret.chars().count() >= MIN_SECRET_CHARS)
        .collect();
    vars.into_iter()
        .filter(|(name, value)| {
            let by_name = name.to_str().is_some_and(is_secret);
            // Lossy, because a value that is not UTF-8 as a whole can still
            // contain a secret's bytes.
            let value = value.to_string_lossy();
            by_name || withheld.iter().any(|secret| value.contains(secret))
        })
        .map(|(name, _)| name)
        .collect()
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

    fn os(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    #[test]
    fn a_variable_holding_a_configured_secret_is_withheld_whatever_its_name() {
        let webhook = "https://ntfy.sh/private-topic-789";
        let removed = withheld_names(
            os(&[
                ("AGENT_NOTIFY_WEBHOOK_URL", webhook),
                ("COPIED_SOMEWHERE", "see private-topic-789 for alerts"),
                ("PATH", "/usr/bin:/bin"),
                ("HOME", "/home/u"),
                ("GITHUB_TOKEN", "anything"),
            ]),
            &[
                webhook.to_string(),
                "private-topic-789".to_string(),
                // A short part must not take ordinary variables with it.
                "/".to_string(),
            ],
        );
        assert_eq!(
            removed,
            [
                "AGENT_NOTIFY_WEBHOOK_URL",
                "COPIED_SOMEWHERE",
                "GITHUB_TOKEN"
            ]
            .map(OsString::from),
        );
    }

    #[test]
    fn without_configured_secrets_only_names_decide() {
        let removed = withheld_names(
            os(&[
                ("AGENT_NOTIFY_WEBHOOK_URL", "https://x"),
                ("DB_PASSWORD", "p"),
            ]),
            &[],
        );
        assert_eq!(removed, [OsString::from("DB_PASSWORD")]);
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
}
