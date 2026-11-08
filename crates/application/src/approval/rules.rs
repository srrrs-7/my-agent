//! The operator's permission rules, applied at the approval gate.
//!
//! The rules themselves - their language and which one wins - are the
//! domain's ([`PermissionRules`]). What is decided here is what a call is
//! *about*: which argument names its file, host or command, and which file a
//! write really reaches. That depends on the tool, so it is a use-case
//! concern, and it is looked up by tool name - a rule names its tool, so
//! there is nothing else to look it up by.
//!
//! The rules never touch the dependency question: whether a dependency file
//! may change is asked separately, of the gate inside, whatever a rule says.

use std::sync::Arc;

use agent_domain::error::{ApprovalError, DomainError};
use agent_domain::model::permission::{
    Effect, PermissionRules, Subject, SubjectKind, Verdict, url_host,
};
use agent_domain::model::tool::{ToolCall, ToolName};
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};
use agent_domain::ports::approval::{
    ApprovalDecision, ApprovalGate, ApprovalRequest, DependencyApprovalRequest,
};
use async_trait::async_trait;
use serde_json::Value;

use crate::tools::ToolRegistry;

/// Tools whose calls name a path in their `path` argument.
const PATH_TOOLS: [&str; 6] = [
    "read_file",
    "write_file",
    "edit_file",
    "list_directory",
    "search_files",
    "glob",
];

/// Built-in tools a rule may name although they are not enabled in this
/// session: an operator's rules are written once for every session, and
/// `web_fetch` or `run_command` is off in most of them.
///
/// `spawn_agent` is here for a second reason: it is never in the registry
/// this gate is built with. The sub-agent it starts asks this same gate, so
/// the gate exists before the tool does (see the composition root).
const BUILT_IN: [&str; 5] = [
    "todo_write",
    "web_fetch",
    "web_search",
    "run_command",
    crate::tools::agent::SPAWN_AGENT,
];

const RUN_COMMAND: &str = "run_command";

/// What calls of `tool` are about, if rules can compare it.
fn subject_kind(tool: &ToolName) -> Option<SubjectKind> {
    match tool.as_str() {
        name if PATH_TOOLS.contains(&name) => Some(SubjectKind::Path),
        "web_fetch" => Some(SubjectKind::Domain),
        RUN_COMMAND => Some(SubjectKind::Command),
        _ => None,
    }
}

/// Applies the operator's rules before `inner` is consulted: `deny` refuses
/// without asking, `ask` makes `inner` ask a human whatever its policy, and
/// `allow` approves without asking. Calls no rule decides go to `inner`
/// unchanged.
pub struct RuleGate {
    inner: Arc<dyn ApprovalGate>,
    rules: PermissionRules,
    tools: Arc<ToolRegistry>,
    root: Arc<WorkspaceRoot>,
}

impl RuleGate {
    /// Checks every rule against the tools there are, so that a rule that
    /// could never match - a misspelt tool, a `domain` on a file tool - stops
    /// startup instead of quietly protecting nothing.
    ///
    /// `tools` is the registry the dispatcher runs: a write's real target is
    /// worked out with the same [`Tool::preview`] the dependency check uses.
    ///
    /// [`Tool::preview`]: agent_domain::ports::tool::Tool::preview
    pub fn new(
        inner: Arc<dyn ApprovalGate>,
        rules: PermissionRules,
        tools: Arc<ToolRegistry>,
        root: Arc<WorkspaceRoot>,
    ) -> Result<Self, DomainError> {
        for rule in rules.rules() {
            let name = rule.tool.as_str();
            let invalid = |reason: String| {
                Err(DomainError::invalid(
                    "permission rule",
                    format!("`{rule}`: {reason}"),
                ))
            };
            if tools.get(&rule.tool).is_none()
                && subject_kind(&rule.tool).is_none()
                && !BUILT_IN.contains(&name)
            {
                return invalid(format!(
                    "there is no tool named `{name}` (enabled: {}). Fix the name or remove the \
                     rule",
                    tools.names().join(", ")
                ));
            }
            if let Some(subject) = &rule.subject {
                match subject_kind(&rule.tool) {
                    Some(kind) if kind == subject.kind() => {}
                    Some(kind) => {
                        return invalid(format!(
                            "`{name}` rules compare a `{}`, not a `{}`",
                            kind.field(),
                            subject.kind().field()
                        ));
                    }
                    None => {
                        return invalid(format!(
                            "`{name}` calls cannot be told apart by a `{}`. Leave it out to \
                             cover every call of the tool",
                            subject.kind().field()
                        ));
                    }
                }
            }
            if name == RUN_COMMAND && rule.effect == Effect::Allow {
                return invalid(
                    "commands can only be denied or asked about. A prefix cannot tell what a \
                     command line will do, so it cannot vouch for one - use AGENT_APPROVAL to \
                     decide how commands are approved"
                        .to_string(),
                );
            }
        }
        Ok(Self {
            inner,
            rules,
            tools,
            root,
        })
    }

    /// What `call` is about, as far as the rules for its tool look.
    async fn subjects(&self, call: &ToolCall) -> Vec<Subject> {
        let Some(kind) = subject_kind(&call.name) else {
            return Vec::new();
        };
        if !self.rules.compares(&call.name, kind) {
            return Vec::new();
        }
        let argument = |name: &str| call.arguments.get(name).filter(|value| !value.is_null());
        match kind {
            SubjectKind::Path => self.paths(call, argument("path")).await,
            SubjectKind::Domain => match argument("url") {
                None => Vec::new(),
                Some(url) => vec![
                    url.as_str()
                        .and_then(url_host)
                        .map_or(Subject::Unreadable(kind), Subject::Host),
                ],
            },
            SubjectKind::Command => match argument("command") {
                None => Vec::new(),
                Some(Value::String(line)) => vec![Subject::Command(line.clone())],
                Some(_) => vec![Subject::Unreadable(kind)],
            },
        }
    }

    /// The path the call names, resolved as the file tools resolve it, and
    /// every file its writes really reach. A path the tool would refuse is
    /// unreadable: it meets every `deny`, and the call fails anyway.
    async fn paths(&self, call: &ToolCall, named: Option<&Value>) -> Vec<Subject> {
        let mut subjects = vec![match named {
            // Optional for the listing tools, where it means the root.
            None => Subject::Path(WorkspacePath::root()),
            Some(Value::String(raw)) => self
                .root
                .resolve(raw)
                .map_or(Subject::Unreadable(SubjectKind::Path), Subject::Path),
            Some(_) => Subject::Unreadable(SubjectKind::Path),
        }];
        // An error here is the dispatcher's to report: arguments `execute`
        // rejects anyway, or a write it refuses to run unchecked.
        let writes = match self.tools.get(&call.name) {
            Some(tool) => tool.preview(&call.arguments).await.unwrap_or_default(),
            None => Vec::new(),
        };
        for write in writes {
            subjects.push(Subject::Path(write.path));
            subjects.push(Subject::Path(write.target));
        }
        subjects
    }
}

/// What "the same thing another way" means for the model, by subject.
fn reached(kind: Option<SubjectKind>) -> &'static str {
    match kind {
        Some(SubjectKind::Path) => "reach the same file",
        Some(SubjectKind::Domain) => "reach the same host",
        Some(SubjectKind::Command) => "run the same command",
        None => "get the same result",
    }
}

#[async_trait]
impl ApprovalGate for RuleGate {
    async fn authorize(
        &self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        if !self.rules.mentions(&request.call.name) {
            return self.inner.authorize(request).await;
        }
        let subjects = self.subjects(&request.call).await;
        match self.rules.evaluate(&request.call.name, &subjects) {
            Verdict::Deny(rule) => Ok(ApprovalDecision::Deny {
                reason: format!(
                    "the operator's permission rule `{rule}` refuses this call. It is a standing \
                     rule, not the user's answer, and asking again will not change it - do not \
                     {} another way (another tool, a command). Continue without it, or tell \
                     the user what you need and why so they can change the rules",
                    reached(subject_kind(&request.call.name))
                ),
            }),
            Verdict::Ask(_) => {
                let request = ApprovalRequest {
                    must_ask: true,
                    ..request.clone()
                };
                self.inner.authorize(&request).await
            }
            Verdict::Allow => Ok(ApprovalDecision::Approve),
            Verdict::Unruled => self.inner.authorize(request).await,
        }
    }

    async fn authorize_dependency_change(
        &self,
        request: &DependencyApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        self.inner.authorize_dependency_change(request).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use agent_domain::error::ToolError;
    use agent_domain::model::access::PathPattern;
    use agent_domain::model::dependency::{
        DependencyChangeDescription, ManifestChange, ManifestKind,
    };
    use agent_domain::model::permission::{
        CommandPrefix, DomainPattern, PermissionRule, SubjectPattern,
    };
    use agent_domain::model::tool::{
        Before, PlannedWrite, ToolCallId, ToolDefinition, ToolOutcome, ToolSafety,
    };
    use agent_domain::ports::tool::Tool;
    use serde_json::json;

    use super::*;

    /// Approves everything, and remembers each request it was handed.
    #[derive(Default)]
    struct Recording {
        seen: Mutex<Vec<ApprovalRequest>>,
        dependency_questions: Mutex<usize>,
    }

    #[async_trait]
    impl ApprovalGate for Recording {
        async fn authorize(
            &self,
            request: &ApprovalRequest,
        ) -> Result<ApprovalDecision, ApprovalError> {
            self.seen.lock().unwrap().push(request.clone());
            Ok(ApprovalDecision::Approve)
        }

        async fn authorize_dependency_change(
            &self,
            _request: &DependencyApprovalRequest,
        ) -> Result<ApprovalDecision, ApprovalError> {
            *self.dependency_questions.lock().unwrap() += 1;
            Ok(ApprovalDecision::Approve)
        }
    }

    /// A `write_file` whose every write lands, through a link, in `target`.
    struct LinkedWrite {
        target: &'static str,
    }

    #[async_trait]
    impl Tool for LinkedWrite {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: ToolName::new("write_file").unwrap(),
                description: String::new(),
                input_schema: json!({}),
                safety: ToolSafety::Mutating,
            }
        }

        async fn preview(&self, arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
            let path = arguments["path"].as_str().unwrap_or_default();
            Ok(vec![PlannedWrite {
                path: WorkspacePath::parse(path).unwrap(),
                target: WorkspacePath::parse(self.target).unwrap(),
                before: Before::Absent,
                after: String::new(),
            }])
        }

        async fn execute(&self, _arguments: Value) -> Result<ToolOutcome, ToolError> {
            unreachable!("the gate never runs a tool")
        }
    }

    fn registry(target: &'static str) -> Arc<ToolRegistry> {
        Arc::new(ToolRegistry::new().with(Arc::new(LinkedWrite { target })))
    }

    fn root() -> Arc<WorkspaceRoot> {
        Arc::new(WorkspaceRoot::new("/workspace").unwrap())
    }

    fn rule(tool: &str, subject: Option<SubjectPattern>, effect: Effect) -> PermissionRule {
        PermissionRule {
            tool: ToolName::new(tool).unwrap(),
            subject,
            effect,
        }
    }

    fn path(raw: &str) -> Option<SubjectPattern> {
        Some(SubjectPattern::Path(PathPattern::parse(raw).unwrap()))
    }

    fn gate(rules: Vec<PermissionRule>, tools: Arc<ToolRegistry>) -> (RuleGate, Arc<Recording>) {
        let inner = Arc::new(Recording::default());
        let gate = RuleGate::new(inner.clone(), PermissionRules::new(rules), tools, root())
            .expect("valid rules");
        (gate, inner)
    }

    fn request(tool: &str, arguments: Value) -> ApprovalRequest {
        ApprovalRequest {
            call: ToolCall::new(
                ToolCallId::new("1"),
                ToolName::new(tool).unwrap(),
                arguments,
            ),
            safety: ToolSafety::Mutating,
            summary: tool.to_string(),
            must_ask: false,
        }
    }

    fn denial(decision: ApprovalDecision) -> String {
        match decision {
            ApprovalDecision::Deny { reason } => reason,
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_strictest_matching_rule_decides() {
        let (gate, inner) = gate(
            vec![
                rule("write_file", path("src/"), Effect::Allow),
                rule("write_file", path("src/gen/"), Effect::Ask),
                rule("write_file", path("src/gen/locked/"), Effect::Deny),
            ],
            Arc::new(ToolRegistry::new()),
        );

        let write = |file: &str| request("write_file", json!({"path": file}));

        let reason = denial(gate.authorize(&write("src/gen/locked/a.rs")).await.unwrap());
        assert!(
            reason.contains("deny write_file path `src/gen/locked/`"),
            "the model is told which rule: {reason}"
        );
        assert!(reason.contains("reach the same file"), "{reason}");
        assert!(
            inner.seen.lock().unwrap().is_empty(),
            "a denial asks nobody"
        );

        gate.authorize(&write("src/gen/a.rs")).await.unwrap();
        assert!(
            inner.seen.lock().unwrap()[0].must_ask,
            "ask reaches the human gate as must_ask"
        );

        assert!(
            gate.authorize(&write("src/lib.rs"))
                .await
                .unwrap()
                .is_approved()
        );
        assert_eq!(inner.seen.lock().unwrap().len(), 1, "allow asks nobody");

        gate.authorize(&write("README.md")).await.unwrap();
        let seen = inner.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(!seen[1].must_ask, "an unruled call goes on unchanged");
    }

    #[tokio::test]
    async fn a_write_through_a_link_is_judged_by_where_it_lands() {
        let (gate, _) = gate(
            vec![
                rule("write_file", path("notes/"), Effect::Allow),
                rule("write_file", path("secrets/"), Effect::Deny),
            ],
            registry("secrets/token"),
        );
        let reason = denial(
            gate.authorize(&request("write_file", json!({"path": "notes/today.md"})))
                .await
                .unwrap(),
        );
        assert!(reason.contains("secrets/"), "{reason}");
    }

    #[tokio::test]
    async fn an_absolute_path_inside_the_workspace_is_matched_like_a_relative_one() {
        let (gate, _) = gate(
            vec![rule("read_file", path("private/"), Effect::Deny)],
            Arc::new(ToolRegistry::new()),
        );
        let decision = gate
            .authorize(&request(
                "read_file",
                json!({"path": "/workspace/private/../private/a.txt"}),
            ))
            .await
            .unwrap();
        denial(decision);
    }

    #[tokio::test]
    async fn hosts_and_commands_are_compared_as_such() {
        let (gate, inner) = gate(
            vec![
                rule(
                    "web_fetch",
                    Some(SubjectPattern::Domain(
                        DomainPattern::parse("evil.example").unwrap(),
                    )),
                    Effect::Deny,
                ),
                rule(
                    "run_command",
                    Some(SubjectPattern::Command(
                        CommandPrefix::parse("git push").unwrap(),
                    )),
                    Effect::Ask,
                ),
            ],
            Arc::new(ToolRegistry::new()),
        );

        let fetch = |url: &str| request("web_fetch", json!({"url": url}));
        denial(
            gate.authorize(&fetch("https://api.EVIL.example/x"))
                .await
                .unwrap(),
        );
        denial(
            gate.authorize(&fetch("https://evil.example%2Fdocs.rs/"))
                .await
                .unwrap(),
        );
        assert!(
            gate.authorize(&fetch("https://docs.rs/"))
                .await
                .unwrap()
                .is_approved()
        );

        gate.authorize(&request(
            "run_command",
            json!({"command": "cargo test && git push"}),
        ))
        .await
        .unwrap();
        let seen = inner.seen.lock().unwrap();
        assert!(seen.last().unwrap().must_ask, "{seen:?}");
    }

    #[test]
    fn a_rule_that_would_allow_commands_stops_startup() {
        for subject in [
            None,
            Some(SubjectPattern::Command(CommandPrefix::parse("ls").unwrap())),
        ] {
            let error = RuleGate::new(
                Arc::new(Recording::default()),
                PermissionRules::new(vec![rule("run_command", subject, Effect::Allow)]),
                Arc::new(ToolRegistry::new()),
                root(),
            )
            .err()
            .expect("refused");
            assert!(
                error.to_string().contains("only be denied or asked about"),
                "{error}"
            );
        }
    }

    #[test]
    fn a_rule_that_could_never_match_stops_startup() {
        let refused = |rule: PermissionRule| {
            RuleGate::new(
                Arc::new(Recording::default()),
                PermissionRules::new(vec![rule]),
                Arc::new(ToolRegistry::new()),
                root(),
            )
            .err()
            .map(|error| error.to_string())
        };
        let typo = refused(rule("wirte_file", None, Effect::Deny)).expect("unknown tool");
        assert!(typo.contains("no tool named `wirte_file`"), "{typo}");

        let wrong_kind = refused(rule(
            "read_file",
            Some(SubjectPattern::Domain(
                DomainPattern::parse("a.example").unwrap(),
            )),
            Effect::Deny,
        ))
        .expect("a domain on a file tool");
        assert!(wrong_kind.contains("compare a `path`"), "{wrong_kind}");

        let no_subject = refused(rule("web_search", path("x"), Effect::Deny))
            .expect("web_search has no subject");
        assert!(no_subject.contains("Leave it out"), "{no_subject}");

        assert_eq!(
            refused(rule("web_fetch", None, Effect::Ask)),
            None,
            "a built-in tool that is off in this session may still be named"
        );
    }

    #[tokio::test]
    async fn the_dependency_question_is_forwarded_whatever_the_rules_say() {
        let (gate, inner) = gate(
            vec![rule("edit_file", None, Effect::Deny)],
            Arc::new(ToolRegistry::new()),
        );
        let request = DependencyApprovalRequest {
            call: request("edit_file", json!({})).call,
            safety: ToolSafety::Mutating,
            changes: vec![ManifestChange {
                path: WorkspacePath::parse("Cargo.toml").unwrap(),
                target: None,
                kind: ManifestKind::CargoToml,
                before: Before::Absent,
                description: DependencyChangeDescription::Lines {
                    reason: "new file".into(),
                    added: vec!["[dependencies]".into()],
                    removed: Vec::new(),
                    omitted: 0,
                },
            }],
        };
        assert!(
            gate.authorize_dependency_change(&request)
                .await
                .unwrap()
                .is_approved()
        );
        assert_eq!(*inner.dependency_questions.lock().unwrap(), 1);
    }
}
