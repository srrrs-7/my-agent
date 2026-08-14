//! Environment scrubbing for child processes.
//!
//! The cheapest exfiltration route out of a sandboxed command is the
//! environment it inherits: an API key in `AGENT_API_KEY` needs no filesystem
//! access and no clever trick, just `echo $AGENT_API_KEY`. Scrubbing costs
//! nothing, works identically on every platform, and does not depend on any
//! sandbox being available - so it runs even in `SandboxKind::None`.

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

/// True when a variable must not reach a child process.
pub(crate) fn is_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_EXACT.contains(&upper.as_str())
        || SECRET_SUFFIXES.iter().any(|suffix| upper.ends_with(suffix))
}

/// Removes every secret-looking variable from a child's environment.
pub(crate) fn scrub(command: &mut tokio::process::Command) {
    for (name, _) in std::env::vars() {
        if is_secret(&name) {
            command.env_remove(&name);
        }
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
    fn a_provider_alias_key_is_a_secret_but_the_alias_list_is_not() {
        // Both start with AGENT_PROVIDER; only one carries a credential.
        assert!(is_secret("AGENT_PROVIDER_CLOUD_API_KEY"));
        assert!(!is_secret("AGENT_PROVIDERS"));
    }
}
