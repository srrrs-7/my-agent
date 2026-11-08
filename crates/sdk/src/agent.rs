//! The embedder's entry point: build an agent, run turns on it.

use std::path::Path;
use std::sync::Arc;

use agent_application::agent::{AgentOutcome, Session as LoopSession};
use agent_domain::model::llm::ModelId;
use agent_domain::ports::approval::ApprovalGate;
use agent_domain::ports::events::{EventSink, FinishReason, NullEventSink};

use crate::approval::PolicyGate;
use crate::events::UsageDto;
use crate::unstable::{self, Assembly, Wiring};
use crate::{Config, Error};

/// Collects what an [`Agent`] is built from. Only the [`Config`] is
/// required.
pub struct AgentBuilder {
    config: Config,
    approval: Option<Arc<dyn ApprovalGate>>,
    events: Option<Arc<dyn EventSink>>,
    model: Option<String>,
}

impl AgentBuilder {
    /// Who is asked about the calls `AGENT_APPROVAL` does not answer in
    /// advance. Without one they are denied; see [`crate::approval`].
    pub fn approval(mut self, gate: Arc<dyn ApprovalGate>) -> Self {
        self.approval = Some(gate);
        self
    }

    /// Where the agent narrates what it does. Without one, nothing is
    /// narrated. [`crate::events::EventDto`] is the stable view of each
    /// event.
    pub fn events(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.events = Some(sink);
        self
    }

    /// The model every request uses, instead of the configured one - what
    /// `--model` does in the binary.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Wires the agent: the same wiring, decorators and sandboxes as the
    /// `agent` binary.
    ///
    /// Fails, rather than starting weaker, when the configuration asks for
    /// something that cannot be had - a command sandbox the platform lacks,
    /// an MCP server that does not start, a permissions file that does not
    /// parse. No model request is made.
    ///
    /// Needs a Tokio runtime: the command sandbox's egress proxy and the MCP
    /// servers are started on it.
    pub async fn build(self) -> Result<Agent, Error> {
        let Self {
            config,
            approval,
            events,
            model,
        } = self;
        let (settings, environment) = config.into_parts();
        let (assembly, ()) = unstable::assemble(
            settings,
            Wiring {
                events: events.unwrap_or_else(|| Arc::new(NullEventSink)),
                model: model.as_deref().map(ModelId::new),
                environment,
                // Housekeeping of the session directory is the host's call;
                // a library does not delete files on construction.
                retention: None,
                asks: Box::new(move |context| {
                    let gate = PolicyGate::new(
                        context.settings.approval,
                        context.settings.allow_dependency_changes,
                        approval,
                    );
                    Box::pin(async move { Ok((Arc::new(gate) as Arc<dyn ApprovalGate>, ())) })
                }),
            },
        )
        .await
        .map_err(Error::new)?;
        Ok(Agent { assembly })
    }
}

/// A wired agent over one workspace.
///
/// Holds what the wiring started - the command sandbox's proxy, MCP servers,
/// background jobs - for as long as it lives; dropping it stops them.
pub struct Agent {
    assembly: Assembly,
}

impl Agent {
    pub fn builder(config: Config) -> AgentBuilder {
        AgentBuilder {
            config,
            approval: None,
            events: None,
            model: None,
        }
    }

    /// One turn in a fresh session.
    ///
    /// `Ok` whenever the provider answered, however the turn ended - see
    /// [`Outcome::reason`]. A tool that failed or was denied is not an
    /// error: the model was told and carried on. `Err` means the provider
    /// itself could not be used.
    pub async fn run(&self, prompt: impl Into<String>) -> Result<Outcome, Error> {
        self.session().send(prompt).await
    }

    /// [`Self::run`], stopping early once `interrupt` fires.
    pub async fn run_until(
        &self,
        prompt: impl Into<String>,
        interrupt: &Interrupt,
    ) -> Result<Outcome, Error> {
        self.session().send_until(prompt, interrupt).await
    }

    /// A conversation that keeps its history from one turn to the next, as
    /// `agent chat` does. It is written to the session log under its own id
    /// (unless `AGENT_SESSION_LOG=false`).
    pub fn session(&self) -> Session<'_> {
        Session {
            agent: self,
            inner: LoopSession::new(uuid::Uuid::new_v4().to_string()),
        }
    }

    /// The names of the tools the model is offered.
    pub fn tool_names(&self) -> Vec<String> {
        self.assembly.tools.names()
    }

    /// The workspace the agent works in, canonicalised.
    pub fn workspace(&self) -> &Path {
        self.assembly.workspace.as_path()
    }

    /// Everything the wiring produced. Not stable; see [`crate::unstable`].
    #[doc(hidden)]
    pub fn assembly(&self) -> &Assembly {
        &self.assembly
    }
}

/// A conversation with an [`Agent`]. Turns run one at a time, each seeing
/// every earlier one.
pub struct Session<'a> {
    agent: &'a Agent,
    inner: LoopSession,
}

impl Session<'_> {
    /// Names the session's record in the session log.
    pub fn id(&self) -> &str {
        &self.inner.id
    }

    /// Tokens used by every turn so far.
    pub fn usage(&self) -> UsageDto {
        (&self.inner.usage).into()
    }

    /// Runs `prompt` as the next turn.
    pub async fn send(&mut self, prompt: impl Into<String>) -> Result<Outcome, Error> {
        self.send_until(prompt, &Interrupt::new()).await
    }

    /// [`Self::send`], stopping early once `interrupt` fires. An interrupted
    /// turn is `Ok` with [`Reason::Interrupted`], and the session stays
    /// usable: every tool call the model made has its answer.
    pub async fn send_until(
        &mut self,
        prompt: impl Into<String>,
        interrupt: &Interrupt,
    ) -> Result<Outcome, Error> {
        let assembly = &self.agent.assembly;
        let session = &mut self.inner;
        if let Some(hooks) = &assembly.hooks {
            hooks.begin_turn(&session.id);
        }
        let result = assembly
            .agent
            .run_until(session, prompt.into(), &interrupt.inner)
            .await;
        // Awaited: a stop hook is the operator's "the turn is over" and must
        // not still be running into the next one. Each is bounded by its own
        // timeout.
        if let Some(hooks) = &assembly.hooks {
            hooks
                .stop(&session.id, unstable::stop_reason(&result))
                .await;
        }
        let outcome = result.map_err(Error::new)?;
        Ok(Outcome::new(outcome, &session.id))
    }
}

/// Asks a running turn to stop.
///
/// The turn stops where it can leave a well-formed history behind - between
/// iterations, mid-request, or once the tool calls already handed out are
/// answered - so the session it belongs to can carry on. Clones share one
/// switch; once fired it stays fired, so use a fresh one per turn.
#[derive(Debug, Clone, Default)]
pub struct Interrupt {
    inner: agent_application::Interrupt,
}

impl Interrupt {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the turn to stop. Safe to call from any thread, any number of
    /// times.
    pub fn fire(&self) {
        self.inner.fire();
    }

    pub fn is_fired(&self) -> bool {
        self.inner.is_fired()
    }
}

/// How one turn went.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Outcome {
    /// The model's answer. Empty when it gave none (out of iterations, or
    /// interrupted before it spoke).
    pub final_text: String,
    pub reason: Reason,
    /// Round trips to the model this turn took.
    pub iterations: u32,
    pub usage: UsageDto,
    /// The session the turn belongs to.
    pub session_id: String,
}

impl Outcome {
    fn new(outcome: AgentOutcome, session_id: &str) -> Self {
        Self {
            reason: reason_of(&outcome.reason),
            usage: (&outcome.usage).into(),
            final_text: outcome.final_text,
            iterations: outcome.iterations,
            session_id: session_id.to_string(),
        }
    }

    /// The model produced its final answer.
    pub fn is_complete(&self) -> bool {
        self.reason == Reason::Completed
    }
}

/// Why a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reason {
    /// The model gave its final answer.
    Completed,
    /// The model was still calling tools when the iteration budget
    /// (`AGENT_MAX_ITERATIONS`) ran out.
    MaxIterations { limit: u32 },
    /// The provider ended the response for its own reason (`max_tokens`, a
    /// content filter), as it named it.
    Stopped { stop_reason: String },
    /// An [`Interrupt`] fired.
    Interrupted,
}

/// A function rather than `From`, which would put the domain type into the
/// public API.
fn reason_of(reason: &FinishReason) -> Reason {
    match reason {
        FinishReason::Completed => Reason::Completed,
        FinishReason::MaxIterations { limit } => Reason::MaxIterations { limit: *limit },
        FinishReason::Stopped { stop_reason } => Reason::Stopped {
            stop_reason: stop_reason.clone(),
        },
        FinishReason::Interrupted => Reason::Interrupted,
    }
}
