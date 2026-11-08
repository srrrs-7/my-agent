//! What a `.env` file may set.
//!
//! `.env` is found by walking up from the current directory, so the one that
//! is read is very often not the operator's: it came with the repository the
//! agent was started in. A file anyone can commit must not be able to hand
//! the agent capabilities the operator never granted - start an MCP server,
//! turn on `run_command`, loosen a sandbox, approve everything, point the
//! session at another workspace. Those settings are taken only from the real
//! environment, which is the operator's own (`env_file` in compose counts:
//! compose sets real variables).
//!
//! Everything else in `.env` is applied as before: only where the real
//! environment does not already have the key.
//!
//! The decision is a pure function of the entries and of what the real
//! environment holds, so it is tested without touching the process
//! environment; `main` does the setting.

/// Keys `.env` cannot set. Compared without regard to case: `http_proxy` is
/// honoured as widely as `HTTP_PROXY`.
///
/// **A new setting that widens what the agent can do, or weakens a
/// confinement, belongs here.** The question to ask is: would a hostile
/// repository want to set this for whoever runs the agent in it?
pub const DENIED: &[&str] = &[
    // Start programs outside the model's tools.
    "AGENT_MCP_CONFIG",
    "AGENT_MCP_SANDBOX",
    // Run commands, and how loosely.
    "AGENT_SHELL",
    "AGENT_SHELL_SANDBOX",
    "AGENT_SHELL_NETWORK_ALLOW",
    "AGENT_SHELL_WRITABLE",
    // Who approves.
    "AGENT_APPROVAL",
    "AGENT_ALLOW_DEPENDENCY_CHANGES",
    // Messages that leave the machine.
    "AGENT_WEB_FETCH",
    "AGENT_WEB_FETCH_ALLOW",
    "AGENT_WEB_FETCH_ALLOW_PRIVATE",
    "AGENT_NOTIFY_WEBHOOK_URL",
    // What the model is told to do, from files outside the workspace.
    "AGENT_GLOBAL_INSTRUCTIONS",
    "AGENT_SYSTEM_PROMPT_FILE",
    "AGENT_APPEND_SYSTEM_PROMPT",
    // Where the file tools reach.
    "AGENT_WORKSPACE",
    // Not the agent's own, but inherited by everything it starts or sends:
    // which binaries run, what is loaded into them, and where HTTPS goes
    // (the API key included) and which certificates vouch for it.
    "PATH",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "BASH_ENV",
    "ENV",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Whether `.env` may set `key`.
pub fn admissible(key: &str) -> bool {
    !DENIED.iter().any(|denied| denied.eq_ignore_ascii_case(key))
}

/// What to do with a `.env` file's entries.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// To set in the process environment, in file order.
    pub set: Vec<(String, String)>,
    /// Denied keys the real environment does not have, which `.env` tried to
    /// set - the operator is told, once each.
    pub ignored: Vec<String>,
}

/// Decides each entry. `present` says whether the real environment already
/// has a key; such a key is never overwritten. Within the file the first
/// occurrence wins, as it does when each entry is set in turn.
pub fn apply(
    entries: impl IntoIterator<Item = (String, String)>,
    present: impl Fn(&str) -> bool,
) -> Applied {
    let mut applied = Applied::default();
    for (key, value) in entries {
        let taken = |key: &str| {
            present(key)
                || applied.set.iter().any(|(set, _)| set == key)
                || applied.ignored.iter().any(|ignored| ignored == key)
        };
        if taken(&key) {
            continue;
        }
        if admissible(&key) {
            applied.set.push((key, value));
        } else {
            applied.ignored.push(key);
        }
    }
    applied
}

/// The one-line warning for `ignored`, or `None` when there is nothing to
/// say.
pub fn warning(ignored: &[String]) -> Option<String> {
    (!ignored.is_empty()).then(|| {
        format!(
            "warning: ignored {} from .env - settings that widen what the agent can do are read \
             only from the real environment",
            ignored.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn a_denied_key_is_ignored_and_named() {
        let applied = apply(
            entries(&[
                ("AGENT_MCP_CONFIG", "/repo/mcp.json"),
                ("AGENT_SHELL_SANDBOX", "none"),
                ("http_proxy", "http://evil.example:8080"),
            ]),
            |_| false,
        );
        assert!(applied.set.is_empty(), "{applied:?}");
        assert_eq!(
            applied.ignored,
            ["AGENT_MCP_CONFIG", "AGENT_SHELL_SANDBOX", "http_proxy"]
        );
        let warning = warning(&applied.ignored).unwrap();
        assert!(warning.contains("AGENT_MCP_CONFIG"), "{warning}");
        assert_eq!(warning.lines().count(), 1, "{warning}");
    }

    #[test]
    fn every_other_key_is_set_in_file_order() {
        let applied = apply(
            entries(&[
                ("AGENT_PROVIDER", "anthropic"),
                ("AGENT_API_KEY", "sk-ant-x"),
                ("AGENT_MODEL", "claude-sonnet-5-5"),
            ]),
            |_| false,
        );
        assert_eq!(
            applied.set,
            entries(&[
                ("AGENT_PROVIDER", "anthropic"),
                ("AGENT_API_KEY", "sk-ant-x"),
                ("AGENT_MODEL", "claude-sonnet-5-5"),
            ])
        );
        assert!(applied.ignored.is_empty());
        assert_eq!(warning(&applied.ignored), None);
    }

    #[test]
    fn the_real_environment_is_never_overwritten() {
        let applied = apply(
            entries(&[("AGENT_MODEL", "from-dotenv"), ("AGENT_SHELL", "true")]),
            |key| key == "AGENT_MODEL" || key == "AGENT_SHELL",
        );
        assert_eq!(applied, Applied::default(), "the real values stand");
    }

    #[test]
    fn the_first_occurrence_in_the_file_wins() {
        let applied = apply(
            entries(&[("AGENT_MODEL", "first"), ("AGENT_MODEL", "second")]),
            |_| false,
        );
        assert_eq!(applied.set, entries(&[("AGENT_MODEL", "first")]));
    }

    #[test]
    fn the_root_decisions_are_all_denied() {
        for key in [
            "AGENT_MCP_CONFIG",
            "AGENT_MCP_SANDBOX",
            "AGENT_SHELL",
            "AGENT_SHELL_SANDBOX",
            "AGENT_SHELL_NETWORK_ALLOW",
            "AGENT_SHELL_WRITABLE",
            "AGENT_APPROVAL",
            "AGENT_WEB_FETCH",
            "AGENT_WEB_FETCH_ALLOW",
            "AGENT_WEB_FETCH_ALLOW_PRIVATE",
            "AGENT_ALLOW_DEPENDENCY_CHANGES",
            "AGENT_GLOBAL_INSTRUCTIONS",
            "AGENT_SYSTEM_PROMPT_FILE",
            "AGENT_APPEND_SYSTEM_PROMPT",
            "AGENT_WORKSPACE",
        ] {
            assert!(!admissible(key), "{key} must not come from .env");
        }
        assert!(admissible("AGENT_BASE_URL"));
        assert!(admissible("AGENT_DENY_READ"), "narrowing access is fine");
    }
}
