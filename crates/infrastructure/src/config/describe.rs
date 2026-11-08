//! The resolved configuration as `agent doctor` shows it, secrets masked.
//!
//! Kept apart from parsing: the two change for different reasons, and what
//! doctor prints is read by a person checking the machine before trusting it
//! with a long session.

use std::collections::BTreeMap;

use crate::session::store::RetentionPolicy;

use super::{NotifySettings, PromptCache, ProviderKind, Settings};

/// Says what will be deleted, in the words of the settings that decide it.
///
/// `doctor` is where someone checks this before trusting the machine with a
/// long session, so "kept forever" has to be as plain as the limits are.
fn describe_retention(policy: &RetentionPolicy) -> String {
    let count = policy
        .keep
        .map(|keep| format!("newest {keep}"))
        .unwrap_or_else(|| "any number".to_string());
    let age = policy
        .max_age
        .map(|max_age| format!("{} days", max_age.as_secs() / (24 * 60 * 60)))
        .unwrap_or_else(|| "forever".to_string());

    format!("{count}, kept {age}")
}

/// Where notifications go and what they carry, with the URL reduced to its
/// host. The excerpt is called out because it is the part that leaves the
/// machine with the model's words in it.
fn describe_notify(notify: &NotifySettings) -> String {
    let Some(url) = &notify.webhook else {
        return "disabled".to_string();
    };
    let events = notify
        .events
        .iter()
        .map(|kind| kind.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let excerpt = if notify.excerpt_chars == 0 {
        "no answer excerpt".to_string()
    } else {
        format!("first {} chars of each answer", notify.excerpt_chars)
    };
    let auth = if notify.ntfy_token.is_some() {
        "; with an access token"
    } else {
        ""
    };
    format!(
        "{} to {} ({events}; turns >= {}s; {excerpt}{auth})",
        notify.format.as_str(),
        url.redacted(),
        notify.min_turn.as_secs()
    )
}

/// Whether a prompt can be answered from the notification. Said plainly,
/// because it is a second way to approve and the operator should never be
/// surprised by it.
fn describe_remote_approval(notify: &NotifySettings) -> String {
    let Some(remote) = &notify.remote_approval else {
        return "disabled (prompts are answered at the terminal only)".to_string();
    };
    let buttons = if remote.reply_token.is_some() {
        "a separate reply token in the buttons"
    } else {
        "the main token in the buttons - set AGENT_NOTIFY_NTFY_REPLY_TOKEN to a write-only one"
    };
    format!(
        "approve/deny from the ntfy notification, which shows the call's arguments with known \
         secrets hidden; tokens expire after {}s; {buttons}; interactive sessions only",
        remote.ttl.as_secs()
    )
}

/// What each provider is sent for `temperature` and `max_tokens`.
///
/// Spelled out per wire protocol because "unset" means different things: the
/// Anthropic API requires `max_tokens` and gets a default, an OpenAI-compatible
/// server gets nothing and decides for itself.
fn describe_generation(settings: &Settings) -> String {
    let anthropic = settings
        .llm
        .providers
        .iter()
        .any(|provider| provider.kind == ProviderKind::Anthropic);
    let temperature = match settings.agent_loop.temperature {
        None => "temperature not sent".to_string(),
        // Sent anyway: the operator asked, and a model that accepts it may
        // be configured. The 400 that follows otherwise is a confusing first
        // failure, hence saying so here.
        Some(value) if anthropic => format!(
            "temperature {value} (WARNING: current Claude models reject `temperature` \
             with HTTP 400 - unset AGENT_TEMPERATURE if requests fail)"
        ),
        Some(value) => format!("temperature {value}"),
    };
    let max_tokens = match settings.agent_loop.max_tokens {
        Some(limit) => format!("max_tokens {limit}"),
        None => format!(
            "max_tokens {} (anthropic) / server default (openai)",
            crate::llm::anthropic::DEFAULT_MAX_TOKENS
        ),
    };
    format!("{temperature}; {max_tokens}")
}

/// Renders the resolved configuration for `agent doctor`, with secrets masked.
pub fn describe(settings: &Settings) -> BTreeMap<String, String> {
    let mut description = BTreeMap::new();
    description.insert("workspace".into(), settings.workspace.display().to_string());
    description.insert("approval".into(), settings.approval.as_str().into());
    description.insert(
        "dependency changes".into(),
        if settings.allow_dependency_changes {
            "ALLOWED without asking (AGENT_ALLOW_DEPENDENCY_CHANGES)".to_string()
        } else {
            "asked every time, whatever the approval policy".to_string()
        },
    );
    description.insert(
        "web fetch".into(),
        if !settings.web_fetch.enabled {
            "disabled".to_string()
        } else {
            let scope = if settings.web_fetch.allowed_domains.is_empty() {
                "any public host".to_string()
            } else {
                format!(
                    "allowlist: {}",
                    settings.web_fetch.allowed_domains.join(", ")
                )
            };
            if settings.web_fetch.allow_private {
                format!("enabled ({scope}; private addresses ALLOWED)")
            } else {
                format!("enabled ({scope})")
            }
        },
    );
    description.insert(
        "web search".into(),
        if settings.web_search.active_key().is_some() {
            "enabled (Brave Search API, key set)".to_string()
        } else {
            "disabled".to_string()
        },
    );
    description.insert(
        "run_command".into(),
        if !settings.shell.enabled {
            "disabled".to_string()
        } else {
            let egress = if settings.shell.allowed_domains.is_empty() {
                "no network".to_string()
            } else {
                format!("allowlist: {}", settings.shell.allowed_domains.join(", "))
            };
            format!("enabled (sandbox: {}; {egress})", settings.shell.sandbox)
        },
    );
    description.insert(
        "mcp servers".into(),
        match &settings.mcp.config {
            None => "disabled".to_string(),
            Some(path) => format!(
                "from {} (sandbox: {})",
                path.display(),
                settings.mcp.sandbox
            ),
        },
    );
    description.insert("notify".into(), describe_notify(&settings.notify));
    description.insert(
        "remote approval".into(),
        describe_remote_approval(&settings.notify),
    );
    description.insert(
        "file access".into(),
        match settings.access_policy(&settings.workspace) {
            Ok(policy) => policy.describe(),
            Err(error) => error.to_string(),
        },
    );
    description.insert(
        "session log".into(),
        if settings.session.log {
            format!(
                "{} (holds the conversation verbatim)",
                settings.session_dir().display()
            )
        } else {
            "disabled".to_string()
        },
    );
    description.insert(
        "session retention".into(),
        describe_retention(&settings.session.retention),
    );
    description.insert(
        "system prompt".into(),
        match (&settings.prompt.replace_file, &settings.prompt.append) {
            (None, None) => "built-in".to_string(),
            (Some(path), None) => format!("replaced from {}", path.display()),
            (None, Some(_)) => "built-in + appended instructions".to_string(),
            (Some(path), Some(_)) => {
                format!("replaced from {} + appended instructions", path.display())
            }
        },
    );
    description.insert(
        "global instructions".into(),
        settings
            .prompt
            .global_instructions
            .as_ref()
            .map_or_else(|| "none".to_string(), |path| path.display().to_string()),
    );
    description.insert(
        "history".into(),
        format!(
            "{} bytes; {}",
            settings.agent_loop.max_history_bytes,
            if settings.agent_loop.compact {
                format!(
                    "compacted into a summary, keeping {} recent messages",
                    settings.agent_loop.compact_keep_recent
                )
            } else {
                "oldest turns dropped".to_string()
            }
        ),
    );
    description.insert("router".into(), settings.llm.router.as_str().into());
    description.extend(settings.llm.routing.describe());
    description.insert(
        "prompt cache".into(),
        match settings.llm.prompt_cache {
            PromptCache::Off => "disabled".to_string(),
            ttl => format!("{} (anthropic providers)", ttl.as_str()),
        },
    );
    description.insert(
        "thinking".into(),
        format!(
            "{}; effort {} (anthropic providers)",
            settings.llm.thinking.as_str(),
            settings
                .llm
                .effort
                .map_or("not sent", |effort| effort.as_str())
        ),
    );
    description.insert("generation".into(), describe_generation(settings));
    description.insert(
        "default provider".into(),
        settings.llm.default_provider.to_string(),
    );
    description.insert(
        "max iterations".into(),
        settings.agent_loop.max_iterations.to_string(),
    );
    for provider in &settings.llm.providers {
        description.insert(
            format!("provider[{}]", provider.id),
            format!(
                "{} model={} base_url={} api_key={}",
                provider.kind.as_str(),
                provider.model,
                provider.base_url,
                provider.masked_api_key()
            ),
        );
    }
    description
}
