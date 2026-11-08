//! Failover across destinations.
//!
//! [`FailoverProvider`] is one routed destination together with the
//! providers that stand in for it. Like [`super::RetryingProvider`] and
//! [`super::RoutingProvider`] it is an [`LlmProvider`] itself, so nothing that
//! calls it knows it is there.
//!
//! ## Where it sits in the stack
//!
//! ```text
//!   RoutingProvider                      <- picks the destination
//!     "cloud" => FailoverProvider(       <- crosses destinations
//!                  Retry(cloud),         <- each destination retried on its own
//!                  [Retry(local)])
//!     "local" => Retry(local)
//! ```
//!
//! Inside the router, because failover is a property of the destination the
//! router chose: the request must already know where it was going before
//! "somewhere else" means anything. Outside the retry layer, because a
//! destination keeps its own retry policy - a blip on the cloud is retried
//! against the cloud, and only once those retries are spent does the request
//! move to another provider. Wrapping the retry layer the other way round
//! would retry the whole chain, so a dead primary would be paid for on every
//! attempt.
//!
//! ## What is worth failing over
//!
//! Exactly what is worth retrying ([`LlmError::is_retryable`]): transport
//! errors, timeouts, 5xx, 408 and 429. A 401 or a 400 is not an outage but a
//! configuration bug, and quietly answering from another provider would hide
//! it until the day the fallback is down too.
//!
//! A stream is never failed over once it has been handed out. An error from
//! [`LlmProvider::chat_stream`] itself means nothing was emitted; an error
//! *inside* the stream means the user has already seen part of an answer, and
//! splicing another provider's answer onto it would show them two.
//!
//! ## Cooldown
//!
//! After the primary fails over, it is skipped for `cooldown` and requests go
//! straight to the fallback - otherwise every request during an outage would
//! pay the primary's full retry latency first. Once the cooldown has passed
//! the primary is tried first again, which is how the destination returns to
//! its first choice. A primary that is cooling down is still tried last, so
//! the cooldown only ever reorders the chain and never makes a request fail
//! that the primary could have served.
//!
//! ## Capabilities
//!
//! [`LlmProvider::capabilities`] reports the primary's. A fallback that cannot
//! do what a request needs - tools, for a request that declares any - is
//! skipped for that request rather than allowed to lower what the destination
//! advertises. A tool-less local model can stand in for summaries without
//! turning tools off for the agent loop.
//!
//! ## Models
//!
//! A fallback is asked for its own configured model. The request's model was
//! chosen for the primary (`-m cloud/claude-...` rewritten to
//! `claude-...`), and sending that name to an Ollama server would only turn
//! an outage into a 404.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use agent_domain::error::LlmError;
use agent_domain::model::llm::{ChatRequest, ChatResponse, ProviderCapabilities, ProviderId};
use agent_domain::ports::llm::{ChatStream, LlmProvider};
use async_trait::async_trait;
use tracing::{info, warn};

/// Source of "now" for the cooldown, so its tests can move time instead of
/// waiting for it.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// The monotonic system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Whether another provider could plausibly serve a request this error failed.
pub fn is_failover_worthy(error: &LlmError) -> bool {
    error.is_retryable()
}

pub struct FailoverProvider {
    /// `[0]` is the primary; the rest are the fallbacks in order.
    candidates: Vec<Arc<dyn LlmProvider>>,
    cooldown: Duration,
    clock: Arc<dyn Clock>,
    /// When the primary last failed over. `None` while it is healthy.
    tripped_at: Mutex<Option<Instant>>,
}

impl FailoverProvider {
    /// A fallback with the primary's id is dropped: failing over to the
    /// provider that just failed would only repeat its retries.
    pub fn new(
        primary: Arc<dyn LlmProvider>,
        fallbacks: Vec<Arc<dyn LlmProvider>>,
        cooldown: Duration,
    ) -> Self {
        let primary_id = primary.id();
        let mut candidates = vec![primary];
        candidates.extend(
            fallbacks
                .into_iter()
                .filter(|fallback| fallback.id() != primary_id),
        );
        Self {
            candidates,
            cooldown,
            clock: Arc::new(SystemClock),
            tripped_at: Mutex::new(None),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    fn id_of(&self, index: usize) -> ProviderId {
        self.candidates[index].id()
    }

    /// How much of the primary's cooldown is left, if any.
    fn cooldown_left(&self) -> Option<Duration> {
        let tripped_at = (*self
            .tripped_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner))?;
        let left = self
            .cooldown
            .checked_sub(self.clock.now().saturating_duration_since(tripped_at))?;
        (!left.is_zero()).then_some(left)
    }

    fn set_tripped(&self, at: Option<Instant>) {
        *self
            .tripped_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = at;
    }

    /// Whether fallback `index` can do what `request` needs.
    fn can_serve(&self, index: usize, request: &ChatRequest) -> bool {
        let needs_tools = request.metadata.requires_tools || !request.tools.is_empty();
        !needs_tools || self.candidates[index].capabilities().supports_tools
    }

    /// The order to try candidates in for this request.
    fn order(&self, request: &ChatRequest) -> Vec<usize> {
        let fallbacks: Vec<usize> = (1..self.candidates.len())
            .filter(|&index| self.can_serve(index, request))
            .collect();

        match self.cooldown_left() {
            Some(left) if !fallbacks.is_empty() => {
                info!(
                    target: "agent::route",
                    "`{}` failed recently; using `{}` for another {}s of cooldown",
                    self.id_of(0),
                    self.id_of(fallbacks[0]),
                    left.as_secs().max(1)
                );
                fallbacks.into_iter().chain([0]).collect()
            }
            _ => [0].into_iter().chain(fallbacks).collect(),
        }
    }

    /// The failover loop both entry points share. `call` is invoked with the
    /// index of the candidate to use and the request to send it.
    async fn run<T, Fut>(
        &self,
        request: ChatRequest,
        call: impl Fn(usize, ChatRequest) -> Fut,
    ) -> Result<T, LlmError>
    where
        Fut: std::future::Future<Output = Result<T, LlmError>>,
    {
        let order = self.order(&request);
        let mut last_error = None;

        for (position, &index) in order.iter().enumerate() {
            let attempt = if index == 0 {
                request.clone()
            } else {
                request.clone().with_model(None)
            };

            match call(index, attempt).await {
                Ok(value) => {
                    if index == 0 {
                        self.set_tripped(None);
                    }
                    return Ok(value);
                }
                Err(error) if is_failover_worthy(&error) => {
                    if index == 0 {
                        self.set_tripped(Some(self.clock.now()));
                    }
                    if let Some(&next) = order.get(position + 1) {
                        // Always visible: someone paying for a cloud model
                        // should know when their answers come from elsewhere.
                        // The reason is coarse on purpose - transport errors
                        // carry the endpoint URL.
                        warn!(
                            "LLM provider `{}` failed ({}); failing over to `{}`",
                            self.id_of(index),
                            coarse_reason(&error),
                            self.id_of(next)
                        );
                    }
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }

        Err(last_error
            .unwrap_or_else(|| LlmError::NoRoute("no provider could serve the request".into())))
    }
}

/// What went wrong, in words safe for a log line: no URL, no response body.
fn coarse_reason(error: &LlmError) -> String {
    match error {
        LlmError::Transport(_) => "connection failed".to_string(),
        LlmError::Timeout { seconds } => format!("timed out after {seconds}s"),
        LlmError::RateLimited { .. } => "rate limited".to_string(),
        LlmError::Api { status, .. } => format!("HTTP {status}"),
        LlmError::Auth(_) => "authentication failed".to_string(),
        LlmError::InvalidResponse(_) => "unreadable response".to_string(),
        LlmError::Unsupported { .. } => "unsupported request".to_string(),
        LlmError::NoRoute(_) => "no route".to_string(),
        LlmError::Configuration(_) => "configuration error".to_string(),
    }
}

#[async_trait]
impl LlmProvider for FailoverProvider {
    fn id(&self) -> ProviderId {
        self.id_of(0)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.candidates[0].capabilities()
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.run(request, |index, request| {
            self.candidates[index].chat(request)
        })
        .await
    }

    /// Fails over only while opening the stream. Once a stream is returned,
    /// its events - errors included - reach the consumer untouched.
    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream, LlmError> {
        self.run(request, |index, request| {
            self.candidates[index].chat_stream(request)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::llm::{ModelId, StopReason, TokenUsage};
    use agent_domain::model::message::Message;
    use agent_domain::model::tool::{ToolDefinition, ToolName, ToolSafety};
    use agent_domain::ports::llm::StreamEvent;
    use futures::StreamExt;
    use std::collections::VecDeque;

    /// Answers from a script; once the script runs out, answers with its name.
    struct Scripted {
        id: ProviderId,
        tools: bool,
        script: Mutex<VecDeque<LlmError>>,
        /// The model each call asked for, in order.
        calls: Mutex<Vec<Option<ModelId>>>,
        /// When set, an opened stream emits a delta and then breaks.
        breaks_mid_stream: bool,
    }

    impl Scripted {
        fn new(id: &str, failures: Vec<LlmError>) -> Arc<Self> {
            Arc::new(Self {
                id: ProviderId::new(id),
                tools: true,
                script: Mutex::new(failures.into()),
                calls: Mutex::new(Vec::new()),
                breaks_mid_stream: false,
            })
        }

        fn healthy(id: &str) -> Arc<Self> {
            Self::new(id, Vec::new())
        }

        fn down(id: &str, error: LlmError) -> Arc<Self> {
            Self::new(id, vec![error; 64])
        }

        fn calls(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        fn answer(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
            self.calls.lock().unwrap().push(request.model.clone());
            if let Some(error) = self.script.lock().unwrap().pop_front() {
                return Err(error);
            }
            Ok(ChatResponse {
                message: Message::assistant_text(self.id.to_string()),
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: ModelId::new("m"),
                provider: self.id.clone(),
            })
        }
    }

    #[async_trait]
    impl LlmProvider for Scripted {
        fn id(&self) -> ProviderId {
            self.id.clone()
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tools: self.tools,
                ..ProviderCapabilities::default()
            }
        }

        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
            self.answer(request)
        }

        async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream, LlmError> {
            let response = self.answer(request)?;
            let events = if self.breaks_mid_stream {
                vec![
                    Ok(StreamEvent::TextDelta("half an ans".into())),
                    Err(LlmError::Transport("connection reset".into())),
                ]
            } else {
                vec![Ok(StreamEvent::Completed(response))]
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// A clock that only moves when told to.
    struct ManualClock {
        start: Instant,
        elapsed: Mutex<Duration>,
    }

    impl ManualClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                start: Instant::now(),
                elapsed: Mutex::new(Duration::ZERO),
            })
        }

        fn advance(&self, by: Duration) {
            *self.elapsed.lock().unwrap() += by;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            self.start + *self.elapsed.lock().unwrap()
        }
    }

    fn failover(primary: Arc<Scripted>, fallback: Arc<Scripted>) -> FailoverProvider {
        FailoverProvider::new(
            primary,
            vec![fallback as Arc<dyn LlmProvider>],
            Duration::from_secs(60),
        )
    }

    fn request() -> ChatRequest {
        ChatRequest::new(vec![Message::user("hi")])
    }

    fn served_by(response: Result<ChatResponse, LlmError>) -> String {
        response.expect("the request should succeed").message.text()
    }

    fn api(status: u16) -> LlmError {
        LlmError::Api {
            status,
            message: "body".into(),
        }
    }

    #[test]
    fn only_errors_another_provider_could_fix_are_failover_worthy() {
        for error in [
            LlmError::Transport("refused".into()),
            LlmError::Timeout { seconds: 180 },
            LlmError::RateLimited {
                retry_after_secs: Some(5),
            },
            api(500),
            api(502),
            api(503),
            api(429),
        ] {
            assert!(is_failover_worthy(&error), "{error} should fail over");
        }
        for error in [
            LlmError::Auth("bad key".into()),
            api(400),
            api(401),
            api(403),
            api(404),
            LlmError::Configuration("x".into()),
        ] {
            assert!(
                !is_failover_worthy(&error),
                "{error} is a configuration bug that failover would hide"
            );
        }
    }

    #[tokio::test]
    async fn a_5xx_from_the_primary_is_served_by_the_fallback() {
        let cloud = Scripted::down("cloud", api(503));
        let local = Scripted::healthy("local");
        let provider = failover(cloud.clone(), local.clone());

        assert_eq!(served_by(provider.chat(request()).await), "local");
        assert_eq!(cloud.calls(), 1);
        assert_eq!(local.calls(), 1);
    }

    #[tokio::test]
    async fn a_transport_error_from_the_primary_is_served_by_the_fallback() {
        let provider = failover(
            Scripted::down("cloud", LlmError::Transport("refused".into())),
            Scripted::healthy("local"),
        );
        assert_eq!(served_by(provider.chat(request()).await), "local");
    }

    #[tokio::test]
    async fn auth_and_bad_request_errors_are_returned_without_failing_over() {
        for error in [LlmError::Auth("bad key".into()), api(400), api(401)] {
            let local = Scripted::healthy("local");
            let provider = failover(Scripted::down("cloud", error.clone()), local.clone());

            let returned = provider.chat(request()).await.unwrap_err();
            assert_eq!(returned.to_string(), error.to_string());
            assert_eq!(local.calls(), 0, "{error} must surface, not be hidden");
        }
    }

    #[tokio::test]
    async fn a_fallback_is_asked_for_its_own_model() {
        let local = Scripted::healthy("local");
        let provider = failover(Scripted::down("cloud", api(503)), local.clone());

        provider
            .chat(request().with_model(Some(ModelId::new("claude-sonnet-5"))))
            .await
            .unwrap();
        assert_eq!(*local.calls.lock().unwrap(), vec![None]);
    }

    #[tokio::test]
    async fn the_last_error_is_returned_when_every_candidate_is_down() {
        let provider = failover(
            Scripted::down("cloud", api(503)),
            Scripted::down("local", LlmError::Transport("refused".into())),
        );
        let error = provider.chat(request()).await.unwrap_err();
        assert!(matches!(error, LlmError::Transport(_)), "{error}");
    }

    #[tokio::test]
    async fn a_tool_less_fallback_is_skipped_for_requests_that_need_tools() {
        let cloud = Scripted::down("cloud", api(503));
        let local = Arc::new(Scripted {
            tools: false,
            ..Arc::into_inner(Scripted::healthy("local")).unwrap()
        });
        let provider = failover(cloud, local.clone());

        let tool = ToolDefinition {
            name: ToolName::new("read_file").unwrap(),
            description: "Read a file.".into(),
            input_schema: serde_json::json!({"type": "object"}),
            safety: ToolSafety::ReadOnly,
        };
        let error = provider
            .chat(request().with_tools(vec![tool]))
            .await
            .unwrap_err();
        assert!(matches!(error, LlmError::Api { status: 503, .. }));
        assert_eq!(local.calls(), 0);

        assert_eq!(
            served_by(provider.chat(request()).await),
            "local",
            "the same fallback still serves requests without tools"
        );
        assert!(
            provider.capabilities().supports_tools,
            "the destination advertises the primary's capabilities"
        );
    }

    #[tokio::test]
    async fn a_stream_that_failed_to_open_fails_over() {
        let provider = failover(
            Scripted::down("cloud", api(503)),
            Scripted::healthy("local"),
        );

        let mut stream = provider.chat_stream(request()).await.unwrap();
        let Some(Ok(StreamEvent::Completed(response))) = stream.next().await else {
            panic!("the fallback's stream should complete");
        };
        assert_eq!(response.provider.as_str(), "local");
    }

    #[tokio::test]
    async fn a_stream_that_broke_after_emitting_is_never_failed_over() {
        let cloud = Arc::new(Scripted {
            breaks_mid_stream: true,
            ..Arc::into_inner(Scripted::healthy("cloud")).unwrap()
        });
        let local = Scripted::healthy("local");
        let provider = failover(cloud, local.clone());

        let events: Vec<_> = provider
            .chat_stream(request())
            .await
            .unwrap()
            .collect()
            .await;

        assert!(matches!(events[0], Ok(StreamEvent::TextDelta(_))));
        assert!(
            matches!(events[1], Err(LlmError::Transport(_))),
            "the break reaches the consumer as it happened: {events:?}"
        );
        assert_eq!(events.len(), 2);
        assert_eq!(
            local.calls(),
            0,
            "a second answer spliced onto the first would show the user both"
        );
    }

    #[tokio::test]
    async fn the_primary_is_skipped_during_the_cooldown_and_tried_again_after_it() {
        let clock = ManualClock::new();
        // Down for the first request only.
        let cloud = Scripted::new("cloud", vec![api(503)]);
        let local = Scripted::healthy("local");
        let provider = failover(cloud.clone(), local.clone()).with_clock(clock.clone());

        assert_eq!(served_by(provider.chat(request()).await), "local");
        assert_eq!(cloud.calls(), 1);

        clock.advance(Duration::from_secs(59));
        assert_eq!(
            served_by(provider.chat(request()).await),
            "local",
            "within the cooldown the request goes straight to the fallback"
        );
        assert_eq!(cloud.calls(), 1, "the primary was not retried");

        clock.advance(Duration::from_secs(1));
        assert_eq!(
            served_by(provider.chat(request()).await),
            "cloud",
            "after the cooldown the destination is back on its first choice"
        );
        assert_eq!(local.calls(), 2);
    }

    #[tokio::test]
    async fn a_cooling_primary_is_still_tried_when_every_fallback_fails() {
        let clock = ManualClock::new();
        let cloud = Scripted::new("cloud", vec![api(503)]);
        let local = Scripted::new("local", vec![]);
        let provider = failover(cloud.clone(), local.clone()).with_clock(clock.clone());
        provider.chat(request()).await.unwrap();

        local.script.lock().unwrap().push_back(api(502));
        assert_eq!(
            served_by(provider.chat(request()).await),
            "cloud",
            "the cooldown reorders the chain; it never removes the primary"
        );
    }

    #[tokio::test]
    async fn a_zero_cooldown_always_tries_the_primary_first() {
        let cloud = Scripted::new("cloud", vec![api(503)]);
        let provider = FailoverProvider::new(
            cloud.clone(),
            vec![Scripted::healthy("local") as Arc<dyn LlmProvider>],
            Duration::ZERO,
        );

        assert_eq!(served_by(provider.chat(request()).await), "local");
        assert_eq!(served_by(provider.chat(request()).await), "cloud");
    }

    #[test]
    fn the_primary_is_never_its_own_fallback() {
        let cloud = Scripted::healthy("cloud");
        let provider = FailoverProvider::new(
            cloud.clone(),
            vec![cloud as Arc<dyn LlmProvider>, Scripted::healthy("local")],
            Duration::ZERO,
        );
        assert_eq!(provider.candidates.len(), 2);
        assert_eq!(provider.id().as_str(), "cloud");
    }

    #[test]
    fn the_logged_reason_never_carries_the_endpoint() {
        let reason = coarse_reason(&LlmError::Transport(
            "error sending request (https://api.example.com/v1?key=sk-secret)".into(),
        ));
        assert!(!reason.contains("example.com"), "{reason}");
        assert!(!reason.contains("sk-secret"), "{reason}");
    }
}
