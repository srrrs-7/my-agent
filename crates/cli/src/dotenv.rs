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
//! Where requests go is the one other thing `.env` may lose. When the real
//! environment holds an API key, every setting that decides the destination
//! of a request - provider, base URL, aliases, routes, failover, model - is
//! taken from the real environment too: otherwise a repository's `.env`
//! could point the operator's own key at a host of its choosing, and the
//! first request would deliver it. When the key comes from `.env` as well,
//! the whole connection is the file's own and is applied as before - that is
//! how a project's own `.env` (`make setup`) is meant to be written.
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
    // Commands that keep running between calls, and for how long.
    "AGENT_SHELL_BACKGROUND",
    "AGENT_SHELL_BACKGROUND_MAX_JOBS",
    "AGENT_SHELL_BACKGROUND_LIFETIME_SECS",
    // Who approves.
    "AGENT_APPROVAL",
    "AGENT_ALLOW_DEPENDENCY_CHANGES",
    // Which calls go through without asking (an `allow` rule), from a file
    // outside the workspace.
    "AGENT_PERMISSIONS",
    // Messages that leave the machine.
    "AGENT_WEB_FETCH",
    "AGENT_WEB_FETCH_ALLOW",
    "AGENT_WEB_FETCH_ALLOW_PRIVATE",
    "AGENT_WEB_SEARCH",
    // With the switch already on in the real environment, a key from `.env`
    // would choose whose account every query is sent under.
    "AGENT_WEB_SEARCH_API_KEY",
    "AGENT_NOTIFY_WEBHOOK_URL",
    // Who may answer an approval from a phone: a token or reply topic from
    // `.env` could hand that to whoever planted it.
    "AGENT_NOTIFY_NTFY_TOKEN",
    "AGENT_NOTIFY_NTFY_REPLY_TOPIC",
    "AGENT_NOTIFY_NTFY_REPLY_TOKEN",
    // What the model is told to do, from files outside the workspace.
    "AGENT_GLOBAL_INSTRUCTIONS",
    "AGENT_SYSTEM_PROMPT_FILE",
    "AGENT_APPEND_SYSTEM_PROMPT",
    // Where the file tools reach.
    "AGENT_WORKSPACE",
    // Where the verbatim conversation is written - and, through retention,
    // which directory's files are deleted.
    "AGENT_SESSION_DIR",
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

/// Keys the provider settings read an API key from: the unaliased one, the
/// vendor fallbacks, and `AGENT_PROVIDER_<ALIAS>_API_KEY` (see
/// `agent_infrastructure::config`). A new place a key is read from belongs
/// here, or a real key in it stops guarding its destination.
const CREDENTIALS: &[&str] = &["AGENT_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"];

/// Keys that decide where a request is sent, compared without regard to case.
/// `AGENT_PROVIDER_*` is matched by prefix below: an alias defined in `.env`
/// would otherwise be a new destination for a vendor fallback key.
///
/// `AGENT_MODEL` and `AGENT_OPENAI_MAX_TOKENS_FIELD` move nothing on their
/// own, but they belong to the same provider block as `AGENT_PROVIDER` - a
/// block read half from each source is one nobody wrote.
const CONNECTION: &[&str] = &[
    "AGENT_PROVIDER",
    "AGENT_BASE_URL",
    "AGENT_MODEL",
    "AGENT_OPENAI_MAX_TOKENS_FIELD",
    "AGENT_PROVIDERS",
    "AGENT_DEFAULT_PROVIDER",
    "AGENT_ROUTER",
];

/// Prefixes of [`CONNECTION`] keys whose suffix is an alias or a task kind.
const CONNECTION_PREFIXES: &[&str] = &["AGENT_PROVIDER_", "AGENT_ROUTE_", "AGENT_FAILOVER"];

fn has_prefix_ignoring_case(key: &str, prefix: &str) -> bool {
    key.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn has_suffix_ignoring_case(key: &str, suffix: &str) -> bool {
    key.len()
        .checked_sub(suffix.len())
        .and_then(|start| key.get(start..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

/// Whether `key` holds an API key for some provider.
pub fn is_credential(key: &str) -> bool {
    CREDENTIALS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(key))
        || (has_prefix_ignoring_case(key, "AGENT_PROVIDER_")
            && has_suffix_ignoring_case(key, "_API_KEY"))
}

/// Whether `key` decides where requests are sent.
pub fn is_connection(key: &str) -> bool {
    CONNECTION
        .iter()
        .any(|known| known.eq_ignore_ascii_case(key))
        || CONNECTION_PREFIXES
            .iter()
            .any(|prefix| has_prefix_ignoring_case(key, prefix))
}

/// Whether the real environment holds an API key. An empty value is not one:
/// the settings read a blank variable as unset, so it sends nothing.
pub fn holds_credential(vars: impl IntoIterator<Item = (String, String)>) -> bool {
    vars.into_iter()
        .any(|(key, value)| is_credential(&key) && !value.trim().is_empty())
}

/// What to do with a `.env` file's entries.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// To set in the process environment, in file order.
    pub set: Vec<(String, String)>,
    /// Denied keys the real environment does not have, which `.env` tried to
    /// set - the operator is told, once each.
    pub ignored: Vec<String>,
    /// Connection keys `.env` tried to set while the real environment holds
    /// an API key - the operator is told, once each.
    pub held_back: Vec<String>,
}

/// Decides each entry. `present` says whether the real environment already
/// has a key; such a key is never overwritten. `real_credential` says
/// whether the real environment holds an API key ([`holds_credential`]); if
/// it does, connection keys are not taken from the file. Within the file the
/// first occurrence wins, as it does when each entry is set in turn.
pub fn apply(
    entries: impl IntoIterator<Item = (String, String)>,
    present: impl Fn(&str) -> bool,
    real_credential: bool,
) -> Applied {
    let mut applied = Applied::default();
    for (key, value) in entries {
        let taken = |key: &str| {
            present(key)
                || applied.set.iter().any(|(set, _)| set == key)
                || applied.ignored.iter().any(|ignored| ignored == key)
                || applied.held_back.iter().any(|held| held == key)
        };
        if taken(&key) {
            continue;
        }
        if !admissible(&key) {
            applied.ignored.push(key);
        } else if real_credential && is_connection(&key) {
            applied.held_back.push(key);
        } else {
            applied.set.push((key, value));
        }
    }
    applied
}

/// The warnings for what was not applied, one line each; empty when there is
/// nothing to say.
pub fn warnings(applied: &Applied) -> Vec<String> {
    let mut warnings = Vec::new();
    if !applied.ignored.is_empty() {
        warnings.push(format!(
            "warning: ignored {} from .env - settings that widen what the agent can do are read \
             only from the real environment",
            applied.ignored.join(", ")
        ));
    }
    if !applied.held_back.is_empty() {
        warnings.push(format!(
            "warning: ignored {} from .env - an API key is set in the real environment, so where \
             requests go is read only from there too; set these in the real environment, or put \
             the API key in .env as well",
            applied.held_back.join(", ")
        ));
    }
    warnings
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
            false,
        );
        assert!(applied.set.is_empty(), "{applied:?}");
        assert_eq!(
            applied.ignored,
            ["AGENT_MCP_CONFIG", "AGENT_SHELL_SANDBOX", "http_proxy"]
        );
        let warnings = warnings(&applied);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("AGENT_MCP_CONFIG"), "{warnings:?}");
        assert_eq!(warnings[0].lines().count(), 1, "{warnings:?}");
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
            false,
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
        assert!(applied.held_back.is_empty());
        assert!(warnings(&applied).is_empty());
    }

    #[test]
    fn the_real_environment_is_never_overwritten() {
        let applied = apply(
            entries(&[("AGENT_MODEL", "from-dotenv"), ("AGENT_SHELL", "true")]),
            |key| key == "AGENT_MODEL" || key == "AGENT_SHELL",
            true,
        );
        assert_eq!(applied, Applied::default(), "the real values stand");
    }

    #[test]
    fn the_first_occurrence_in_the_file_wins() {
        let applied = apply(
            entries(&[("AGENT_MODEL", "first"), ("AGENT_MODEL", "second")]),
            |_| false,
            false,
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
            "AGENT_SHELL_BACKGROUND",
            "AGENT_SHELL_BACKGROUND_MAX_JOBS",
            "AGENT_SHELL_BACKGROUND_LIFETIME_SECS",
            "AGENT_APPROVAL",
            "AGENT_WEB_FETCH",
            "AGENT_WEB_FETCH_ALLOW",
            "AGENT_WEB_FETCH_ALLOW_PRIVATE",
            "AGENT_ALLOW_DEPENDENCY_CHANGES",
            "AGENT_PERMISSIONS",
            "AGENT_GLOBAL_INSTRUCTIONS",
            "AGENT_SYSTEM_PROMPT_FILE",
            "AGENT_APPEND_SYSTEM_PROMPT",
            "AGENT_WORKSPACE",
            "AGENT_SESSION_DIR",
        ] {
            assert!(!admissible(key), "{key} must not come from .env");
        }
        assert!(admissible("AGENT_BASE_URL"));
        assert!(admissible("AGENT_DENY_READ"), "narrowing access is fine");
    }

    /// The real environment holds `AGENT_API_KEY`; everything else is `.env`.
    fn with_a_real_key(pairs: &[(&str, &str)]) -> Applied {
        apply(entries(pairs), |key| key == "AGENT_API_KEY", true)
    }

    #[test]
    fn a_real_key_is_not_sent_where_the_dotenv_points() {
        let applied = with_a_real_key(&[
            ("AGENT_PROVIDER", "openai"),
            ("AGENT_BASE_URL", "https://attacker.example/v1"),
            ("AGENT_MODEL", "gpt-x"),
            ("AGENT_MAX_ITERATIONS", "5"),
        ]);
        assert_eq!(
            applied.held_back,
            ["AGENT_PROVIDER", "AGENT_BASE_URL", "AGENT_MODEL"]
        );
        assert_eq!(
            applied.set,
            entries(&[("AGENT_MAX_ITERATIONS", "5")]),
            "settings unrelated to the destination still apply"
        );
        let warnings = warnings(&applied);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("AGENT_BASE_URL"), "{warnings:?}");
        assert!(
            warnings[0].contains("real environment"),
            "the operator is told where to put it: {warnings:?}"
        );
    }

    #[test]
    fn a_dotenv_cannot_point_an_alias_or_a_new_one_elsewhere() {
        let applied = with_a_real_key(&[
            ("AGENT_PROVIDER_CLOUD_BASE_URL", "https://attacker.example"),
            ("AGENT_PROVIDERS", "evil"),
            ("AGENT_PROVIDER_EVIL_KIND", "openai"),
            (
                "AGENT_PROVIDER_EVIL_BASE_URL",
                "https://attacker.example/v1",
            ),
            ("AGENT_DEFAULT_PROVIDER", "evil"),
            ("AGENT_ROUTER", "task-kind"),
            ("AGENT_ROUTE_AGENTIC", "evil"),
            ("AGENT_FAILOVER", "evil"),
            ("agent_base_url", "https://attacker.example/v1"),
        ]);
        assert!(applied.set.is_empty(), "{applied:?}");
        assert_eq!(applied.held_back.len(), 9, "{applied:?}");
        assert!(warnings(&applied)[0].contains("AGENT_PROVIDERS"));
    }

    #[test]
    fn a_dotenv_with_its_own_key_configures_the_whole_connection() {
        let pairs = [
            ("AGENT_PROVIDER", "openai"),
            ("AGENT_BASE_URL", "http://vllm:8000/v1"),
            ("AGENT_API_KEY", "sk-from-dotenv"),
            ("AGENT_PROVIDERS", "local"),
            ("AGENT_PROVIDER_LOCAL_KIND", "ollama"),
        ];
        let applied = apply(entries(&pairs), |_| false, false);
        assert_eq!(applied.set, entries(&pairs));
        assert!(applied.held_back.is_empty(), "{applied:?}");
    }

    #[test]
    fn credentials_are_recognised_wherever_the_settings_read_them() {
        for key in [
            "AGENT_API_KEY",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "AGENT_PROVIDER_CLOUD_API_KEY",
            "AGENT_PROVIDER_MY_CLOUD_API_KEY",
        ] {
            assert!(
                holds_credential([(key.to_string(), "sk-x".to_string())]),
                "{key}"
            );
        }
        assert!(!holds_credential([(
            "AGENT_API_KEY".to_string(),
            "  ".to_string()
        )]));
        assert!(!holds_credential([(
            "AGENT_BASE_URL".to_string(),
            "http://x".to_string()
        )]));
        assert!(!is_credential("AGENT_PROVIDER_CLOUD_BASE_URL"));
        assert!(
            !is_credential("AGENT_PROVIDER_\u{e9}"),
            "no panic on a short or odd key"
        );
    }

    #[test]
    fn a_dotenv_cannot_turn_on_web_search() {
        let applied = apply(
            entries(&[
                ("AGENT_WEB_SEARCH", "true"),
                ("AGENT_WEB_SEARCH_API_KEY", "BSA-from-the-repo"),
                ("agent_web_search", "1"),
            ]),
            |_| false,
            false,
        );
        assert!(applied.set.is_empty(), "{applied:?}");
        assert_eq!(
            applied.ignored,
            [
                "AGENT_WEB_SEARCH",
                "AGENT_WEB_SEARCH_API_KEY",
                "agent_web_search"
            ]
        );
    }

    #[test]
    fn the_session_dir_never_comes_from_dotenv() {
        let applied = apply(
            entries(&[("AGENT_SESSION_DIR", "/tmp/shared")]),
            |_| false,
            false,
        );
        assert!(applied.set.is_empty(), "{applied:?}");
        assert_eq!(applied.ignored, ["AGENT_SESSION_DIR"]);
    }
}
