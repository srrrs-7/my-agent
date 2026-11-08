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
//!   The rule lives in [`crate::secrets`], shared with the redactor.
//! * **By value**: any variable holding one of the secrets the operator
//!   configured - provider keys, the webhook URL. `AGENT_NOTIFY_WEBHOOK_URL`
//!   carries no secret-looking suffix, yet whoever can read it can post to the
//!   user's phone; matching on what the settings hold catches it, and every
//!   future setting of the kind, without naming it.
use std::ffi::OsString;

use crate::secrets::{MIN_SECRET_CHARS, is_secret};

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
}
