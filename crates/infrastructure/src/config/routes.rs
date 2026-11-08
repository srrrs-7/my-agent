//! Where a request goes when "the default provider" is not the whole answer:
//! per-task destinations and the failover chain.
//!
//! Kept out of `mod.rs` because every rule here is a cross-reference - each
//! alias must name a configured provider, and the per-task routes only mean
//! something to one router - and those checks read better side by side than
//! spread through the general settings.

use std::collections::HashMap;
use std::time::Duration;

use agent_domain::model::llm::{ProviderId, TaskKind};

use super::env::Reader;
use super::{ConfigError, ProviderSettings, RouterKind, names};

const ROUTER_KEY: &str = "AGENT_ROUTER";
const FAILOVER_KEY: &str = "AGENT_FAILOVER";

/// One variable per task kind, in the order `doctor` lists them.
const ROUTE_KEYS: [(TaskKind, &str); 4] = [
    (TaskKind::Agentic, "AGENT_ROUTE_AGENTIC"),
    (TaskKind::Chat, "AGENT_ROUTE_CHAT"),
    (TaskKind::Summarize, "AGENT_ROUTE_SUMMARIZE"),
    (TaskKind::Classify, "AGENT_ROUTE_CLASSIFY"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingSettings {
    /// Destination per task kind, read by the `task-kind` router. A kind that
    /// is absent goes to the default provider.
    pub by_task: HashMap<TaskKind, ProviderId>,
    /// Tried in order when the routed destination fails in a way another
    /// provider could fix. Empty means no failover.
    pub failover: Vec<ProviderId>,
    /// How long a destination that just failed over is skipped before it is
    /// tried first again.
    pub failover_cooldown: Duration,
}

impl Default for RoutingSettings {
    fn default() -> Self {
        Self {
            by_task: HashMap::new(),
            failover: Vec::new(),
            failover_cooldown: Duration::from_secs(60),
        }
    }
}

impl RoutingSettings {
    pub(super) fn read(
        reader: &Reader<'_>,
        providers: &[ProviderSettings],
    ) -> Result<Self, ConfigError> {
        let mut by_task = HashMap::new();
        for (kind, key) in ROUTE_KEYS {
            if let Some(alias) = reader.string(key) {
                by_task.insert(kind, configured(key, &alias, providers)?);
            }
        }

        let mut failover: Vec<ProviderId> = Vec::new();
        for alias in reader.list(FAILOVER_KEY) {
            let id = configured(FAILOVER_KEY, &alias, providers)?;
            if !failover.contains(&id) {
                failover.push(id);
            }
        }

        Ok(Self {
            by_task,
            failover,
            failover_cooldown: Duration::from_secs(
                reader.parsed("AGENT_FAILOVER_COOLDOWN_SECS", 60)?,
            ),
        })
    }

    /// The router to use, given the one `mod.rs` would have picked on its own.
    ///
    /// Per-task routes only mean something to the `task-kind` router. Setting
    /// one is therefore enough to select it, and setting one next to an
    /// explicit `static` or `model-prefix` is an error rather than a route
    /// that silently does nothing.
    pub(super) fn router(
        &self,
        reader: &Reader<'_>,
        fallback: RouterKind,
    ) -> Result<RouterKind, ConfigError> {
        let first_route = ROUTE_KEYS
            .iter()
            .find(|(kind, _)| self.by_task.contains_key(kind))
            .map(|(_, key)| *key);

        match (reader.optional::<RouterKind>(ROUTER_KEY)?, first_route) {
            (None, Some(_)) => Ok(RouterKind::TaskKind),
            (None, None) => Ok(fallback),
            (Some(kind), None) => Ok(kind),
            (Some(RouterKind::TaskKind), Some(_)) => Ok(RouterKind::TaskKind),
            (Some(other), Some(key)) => Err(ConfigError::invalid(
                ROUTER_KEY,
                other.as_str(),
                format!(
                    "{key} is set, but only the task-kind router reads it; set \
                     {ROUTER_KEY}=task-kind or remove {key}"
                ),
            )),
        }
    }

    /// The `doctor` lines for routing: one per configured route, and the
    /// failover chain (shown even when off, since "disabled" is an answer).
    pub(super) fn describe(&self) -> Vec<(String, String)> {
        let mut lines = Vec::new();
        for (kind, key) in ROUTE_KEYS {
            if let Some(id) = self.by_task.get(&kind) {
                lines.push((
                    format!("route[{}]", task_label(kind)),
                    format!("{id} ({key})"),
                ));
            }
        }
        lines.push((
            "failover".to_string(),
            if self.failover.is_empty() {
                "disabled".to_string()
            } else {
                format!(
                    "{} (cooldown {}s)",
                    self.failover
                        .iter()
                        .map(ProviderId::to_string)
                        .collect::<Vec<_>>()
                        .join(" -> "),
                    self.failover_cooldown.as_secs()
                )
            },
        ));
        lines
    }
}

fn task_label(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Agentic => "agentic",
        TaskKind::Chat => "chat",
        TaskKind::Summarize => "summarize",
        TaskKind::Classify => "classify",
    }
}

/// `alias` as a provider id, or an error naming the variable and what exists.
fn configured(
    key: &str,
    alias: &str,
    providers: &[ProviderSettings],
) -> Result<ProviderId, ConfigError> {
    let id = ProviderId::new(alias);
    if providers.iter().any(|provider| provider.id == id) {
        Ok(id)
    } else {
        Err(ConfigError::invalid(
            key,
            alias,
            format!(
                "not among the configured providers: {} (add it to AGENT_PROVIDERS)",
                names(providers)
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{MapEnv, Settings, describe};
    use super::*;

    /// A local Ollama and a cloud Anthropic, plus whatever the test adds.
    fn hybrid(extra: &[(&str, &str)]) -> Result<Settings, ConfigError> {
        let mut env = MapEnv::new(&[
            ("AGENT_WORKSPACE", "/workspace"),
            ("AGENT_PROVIDERS", "local,cloud"),
            ("AGENT_PROVIDER_LOCAL_KIND", "ollama"),
            ("AGENT_PROVIDER_LOCAL_MODEL", "qwen3:8b"),
            ("AGENT_PROVIDER_CLOUD_KIND", "anthropic"),
            ("AGENT_PROVIDER_CLOUD_MODEL", "claude-sonnet-5"),
            ("AGENT_PROVIDER_CLOUD_API_KEY", "sk-ant-x"),
            ("AGENT_DEFAULT_PROVIDER", "cloud"),
        ]);
        for (key, value) in extra {
            env.set(key, value);
        }
        Settings::from_source(&env)
    }

    #[test]
    fn nothing_is_routed_or_failed_over_unless_asked() {
        let settings = hybrid(&[]).unwrap();

        assert_eq!(settings.llm.router, RouterKind::ModelPrefix);
        assert!(settings.llm.routing.by_task.is_empty());
        assert!(
            settings.llm.routing.failover.is_empty(),
            "failover is opt-in: unset must keep today's behaviour"
        );
        assert_eq!(
            settings.llm.routing.failover_cooldown,
            Duration::from_secs(60)
        );
    }

    #[test]
    fn per_kind_routes_are_read_and_select_the_task_kind_router() {
        let settings = hybrid(&[
            ("AGENT_ROUTE_SUMMARIZE", "local"),
            ("AGENT_ROUTE_CLASSIFY", "local"),
        ])
        .unwrap();

        assert_eq!(
            settings.llm.router,
            RouterKind::TaskKind,
            "a route alone is enough to pick the router that reads it"
        );
        let by_task = &settings.llm.routing.by_task;
        assert_eq!(by_task[&TaskKind::Summarize].as_str(), "local");
        assert_eq!(by_task[&TaskKind::Classify].as_str(), "local");
        assert!(
            !by_task.contains_key(&TaskKind::Agentic),
            "an unset kind falls to the default provider"
        );
    }

    #[test]
    fn the_task_kind_router_can_be_named_without_any_route() {
        let settings = hybrid(&[("AGENT_ROUTER", "task-kind")]).unwrap();
        assert_eq!(settings.llm.router, RouterKind::TaskKind);
    }

    #[test]
    fn a_route_next_to_another_router_is_rejected_instead_of_ignored() {
        let error = hybrid(&[
            ("AGENT_ROUTER", "static"),
            ("AGENT_ROUTE_SUMMARIZE", "local"),
        ])
        .expect_err("a route the router never reads must not pass silently");
        let message = error.to_string();
        assert!(message.contains("AGENT_ROUTER"), "{message}");
        assert!(message.contains("AGENT_ROUTE_SUMMARIZE"), "{message}");
        assert!(message.contains("task-kind"), "{message}");
    }

    #[test]
    fn an_unknown_route_alias_names_the_variable_and_the_configured_aliases() {
        let error = hybrid(&[("AGENT_ROUTE_SUMMARIZE", "lcoal")]).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("AGENT_ROUTE_SUMMARIZE"), "{message}");
        assert!(message.contains("lcoal"), "{message}");
        assert!(message.contains("local, cloud"), "{message}");
    }

    #[test]
    fn the_failover_chain_keeps_its_order_and_drops_repeats() {
        let settings = hybrid(&[
            ("AGENT_FAILOVER", "local, cloud,local"),
            ("AGENT_FAILOVER_COOLDOWN_SECS", "5"),
        ])
        .unwrap();

        let chain: Vec<&str> = settings
            .llm
            .routing
            .failover
            .iter()
            .map(ProviderId::as_str)
            .collect();
        assert_eq!(chain, ["local", "cloud"]);
        assert_eq!(
            settings.llm.routing.failover_cooldown,
            Duration::from_secs(5)
        );
    }

    #[test]
    fn an_unknown_failover_alias_aborts_startup() {
        let error = hybrid(&[("AGENT_FAILOVER", "local,backup")]).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("AGENT_FAILOVER"), "{message}");
        assert!(message.contains("backup"), "{message}");
        assert!(message.contains("local, cloud"), "{message}");
    }

    #[test]
    fn a_bad_cooldown_names_the_variable() {
        let error = hybrid(&[("AGENT_FAILOVER_COOLDOWN_SECS", "a minute")]).unwrap_err();
        assert!(
            error.to_string().contains("AGENT_FAILOVER_COOLDOWN_SECS"),
            "{error}"
        );
    }

    #[test]
    fn doctor_shows_routes_and_the_failover_chain() {
        let settings = hybrid(&[
            ("AGENT_ROUTE_SUMMARIZE", "local"),
            ("AGENT_FAILOVER", "local"),
        ])
        .unwrap();
        let rendered = describe(&settings);

        assert_eq!(rendered["router"], "task-kind");
        assert_eq!(
            rendered["route[summarize]"],
            "local (AGENT_ROUTE_SUMMARIZE)"
        );
        assert_eq!(rendered["failover"], "local (cooldown 60s)");
    }
}
