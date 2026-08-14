//! Retry decorator.
//!
//! Lives in the infrastructure layer because backoff needs a timer, and keeping
//! the runtime out of the use-case layer is what lets the agent loop be tested
//! without one.

use std::sync::Arc;
use std::time::Duration;

use agent_domain::error::LlmError;
use agent_domain::model::llm::{ChatRequest, ChatResponse, ProviderCapabilities, ProviderId};
use agent_domain::ports::llm::{ChatStream, LlmProvider};
use async_trait::async_trait;
use tracing::warn;

pub struct RetryingProvider {
    inner: Arc<dyn LlmProvider>,
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
}

impl RetryingProvider {
    pub fn new(inner: Arc<dyn LlmProvider>, max_attempts: u32) -> Self {
        Self {
            inner,
            max_attempts: max_attempts.max(1),
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
        }
    }

    pub fn with_delays(mut self, base: Duration, max: Duration) -> Self {
        self.base_delay = base;
        self.max_delay = max;
        self
    }

    /// Exponential backoff, except when the server told us how long to wait.
    fn delay_for(&self, attempt: u32, error: &LlmError) -> Duration {
        if let LlmError::RateLimited {
            retry_after_secs: Some(seconds),
        } = error
        {
            return Duration::from_secs(*seconds).min(self.max_delay);
        }
        let exponent = attempt.saturating_sub(1).min(6);
        (self.base_delay * 2_u32.pow(exponent)).min(self.max_delay)
    }

    /// The retry loop both entry points share. `call` is invoked once per
    /// attempt with a fresh clone of the request.
    async fn run_with_retry<T, Fut>(
        &self,
        request: ChatRequest,
        call: impl Fn(ChatRequest) -> Fut,
    ) -> Result<T, LlmError>
    where
        Fut: std::future::Future<Output = Result<T, LlmError>>,
    {
        let mut last_error = None;

        for attempt in 1..=self.max_attempts {
            match call(request.clone()).await {
                Ok(value) => return Ok(value),
                Err(error) if error.is_retryable() && attempt < self.max_attempts => {
                    let delay = self.delay_for(attempt, &error);
                    warn!(
                        attempt,
                        max_attempts = self.max_attempts,
                        delay_ms = delay.as_millis(),
                        %error,
                        "retrying after a transient provider failure"
                    );
                    tokio::time::sleep(delay).await;
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }

        Err(last_error
            .unwrap_or_else(|| LlmError::Transport("the provider was never called".to_string())))
    }
}

#[async_trait]
impl LlmProvider for RetryingProvider {
    fn id(&self) -> ProviderId {
        self.inner.id()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.run_with_retry(request, |request| self.inner.chat(request))
            .await
    }

    /// Retries only the *opening* of a stream. Once `chat_stream` has returned
    /// `Ok`, events flow through untouched: an error inside the stream aborts
    /// it, because silently replaying a half-delivered answer would duplicate
    /// whatever the consumer already rendered.
    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream, LlmError> {
        self.run_with_retry(request, |request| self.inner.chat_stream(request))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::llm::{ModelId, StopReason, TokenUsage};
    use agent_domain::model::message::Message;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct FlakyProvider {
        calls: AtomicU32,
        fail_first: u32,
        error: LlmError,
    }

    #[async_trait]
    impl LlmProvider for FlakyProvider {
        fn id(&self) -> ProviderId {
            ProviderId::new("flaky")
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call <= self.fail_first {
                return Err(self.error.clone());
            }
            Ok(ChatResponse {
                message: Message::assistant_text("ok"),
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: ModelId::new("m"),
                provider: ProviderId::new("flaky"),
            })
        }
    }

    fn flaky(fail_first: u32, error: LlmError) -> Arc<FlakyProvider> {
        Arc::new(FlakyProvider {
            calls: AtomicU32::new(0),
            fail_first,
            error,
        })
    }

    fn request() -> ChatRequest {
        ChatRequest::new(vec![Message::user("hi")])
    }

    #[tokio::test]
    async fn recovers_from_transient_failures() {
        let inner = flaky(2, LlmError::Transport("connection reset".into()));
        let provider = RetryingProvider::new(inner.clone(), 3)
            .with_delays(Duration::from_millis(1), Duration::from_millis(2));

        assert_eq!(provider.chat(request()).await.unwrap().message.text(), "ok");
        assert_eq!(inner.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_the_attempt_budget() {
        let inner = flaky(u32::MAX, LlmError::Transport("down".into()));
        let provider = RetryingProvider::new(inner.clone(), 2)
            .with_delays(Duration::from_millis(1), Duration::from_millis(2));

        assert!(provider.chat(request()).await.is_err());
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn does_not_retry_permanent_failures() {
        let inner = flaky(u32::MAX, LlmError::Auth("bad key".into()));
        let provider = RetryingProvider::new(inner.clone(), 5)
            .with_delays(Duration::from_millis(1), Duration::from_millis(2));

        assert!(provider.chat(request()).await.is_err());
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            1,
            "an auth failure is not retryable"
        );
    }

    #[tokio::test]
    async fn honours_retry_after() {
        let provider = RetryingProvider::new(flaky(0, LlmError::Auth("unused".into())), 3);
        let delay = provider.delay_for(
            1,
            &LlmError::RateLimited {
                retry_after_secs: Some(7),
            },
        );
        assert_eq!(delay, Duration::from_secs(7));
    }
}
