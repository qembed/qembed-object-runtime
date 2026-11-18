//! Deterministic in-memory implementation of the domain ChatProvider port.
//!
//! Scripts are selected by exact request equality, never by call order. This
//! keeps tests reproducible when requests execute concurrently. The provider
//! is immutable after construction, so calls need no mutex and share no
//! mutable cursor or global queue.

use std::error::Error;
use std::fmt;

use qembed_domain::chat::{ChatRequest, ChatResponse};
use qembed_domain::ports::ChatProvider;

#[derive(Clone, Debug, PartialEq, Eq)]
enum ScriptedOutcome {
    Response(ChatResponse),
    Error(MockChatProviderError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Script {
    request: ChatRequest,
    outcome: ScriptedOutcome,
}

/// Configuration error returned when the same request is scripted twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockConfigurationError {
    DuplicateRequest,
}

impl fmt::Display for MockConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a response or error is already scripted for this request")
    }
}

impl Error for MockConfigurationError {}

/// Deterministic failures that tests can script without waiting or networking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockChatProviderError {
    /// Simulates a provider request timeout immediately and deterministically.
    Timeout,
    /// Simulates a provider-side availability failure.
    Unavailable,
    /// No script was registered for the exact request.
    UnscriptedRequest,
}

impl fmt::Display for MockChatProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Timeout => "mock provider timed out",
            Self::Unavailable => "mock provider is unavailable",
            Self::UnscriptedRequest => "mock provider has no script for this request",
        };
        formatter.write_str(message)
    }
}

impl Error for MockChatProviderError {}

/// In-memory ChatProvider whose output is determined only by request content.
///
/// Register each request exactly once with the with_response or with_error
/// methods. An unregistered request returns UnscriptedRequest, making missing
/// fixtures visible instead of silently returning a plausible response.
#[derive(Clone, Debug, Default)]
pub struct MockChatProvider {
    scripts: Vec<Script>,
}

impl MockChatProvider {
    /// Creates an empty provider. Every request will fail until scripted.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            scripts: Vec::new(),
        }
    }

    /// Registers the response for one exact request.
    ///
    /// Duplicate request scripts are rejected to avoid order-dependent
    /// ambiguity in tests.
    pub fn with_response(
        mut self,
        request: ChatRequest,
        response: ChatResponse,
    ) -> Result<Self, MockConfigurationError> {
        self.insert(request, ScriptedOutcome::Response(response))?;
        Ok(self)
    }

    /// Registers a deterministic provider failure for one exact request.
    ///
    /// A timeout is represented as an immediate typed error; the mock does not
    /// sleep, depend on a clock, or introduce scheduler-sensitive timing.
    pub fn with_error(
        mut self,
        request: ChatRequest,
        error: MockChatProviderError,
    ) -> Result<Self, MockConfigurationError> {
        self.insert(request, ScriptedOutcome::Error(error))?;
        Ok(self)
    }

    fn insert(
        &mut self,
        request: ChatRequest,
        outcome: ScriptedOutcome,
    ) -> Result<(), MockConfigurationError> {
        if self.scripts.iter().any(|script| script.request == request) {
            return Err(MockConfigurationError::DuplicateRequest);
        }
        self.scripts.push(Script { request, outcome });
        Ok(())
    }
}

impl ChatProvider for MockChatProvider {
    type Error = MockChatProviderError;

    fn complete(
        &self,
        request: ChatRequest,
    ) -> impl std::future::Future<Output = Result<ChatResponse, Self::Error>> + Send {
        async move {
            let script = self
                .scripts
                .iter()
                .find(|script| script.request == request)
                .ok_or(MockChatProviderError::UnscriptedRequest)?;

            match &script.outcome {
                ScriptedOutcome::Response(response) => Ok(response.clone()),
                ScriptedOutcome::Error(error) => Err(*error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use qembed_domain::chat::{ChatFinishReason, ChatMessage, ChatRole, TokenUsage};

    use super::*;

    fn request(prompt: &str) -> ChatRequest {
        ChatRequest::new(
            "test-chat",
            vec![ChatMessage::new(ChatRole::User, prompt).expect("valid message")],
            128,
        )
        .expect("valid request")
    }

    fn response(content: &str) -> ChatResponse {
        ChatResponse::new(
            content,
            ChatFinishReason::Stop,
            Some(TokenUsage::new(11, 5)),
        )
        .expect("valid response")
    }

    #[tokio::test]
    async fn exact_input_returns_the_scripted_output_and_usage() {
        let input = request("hello");
        let provider = MockChatProvider::new()
            .with_response(input.clone(), response("world"))
            .expect("unique script");

        let actual = provider.complete(input).await.expect("scripted response");
        assert_eq!(actual, response("world"));
        assert_eq!(actual.usage(), Some(TokenUsage::new(11, 5)));
    }

    #[tokio::test]
    async fn scripted_timeout_and_provider_error_are_typed_and_immediate() {
        let timeout_request = request("timeout");
        let unavailable_request = request("unavailable");
        let provider = MockChatProvider::new()
            .with_error(timeout_request.clone(), MockChatProviderError::Timeout)
            .expect("unique timeout script")
            .with_error(
                unavailable_request.clone(),
                MockChatProviderError::Unavailable,
            )
            .expect("unique unavailable script");

        assert_eq!(
            provider.complete(timeout_request).await.unwrap_err(),
            MockChatProviderError::Timeout
        );
        assert_eq!(
            provider.complete(unavailable_request).await.unwrap_err(),
            MockChatProviderError::Unavailable
        );
    }

    #[tokio::test]
    async fn missing_and_duplicate_scripts_fail_explicitly() {
        let input = request("hello");
        let provider = MockChatProvider::new()
            .with_response(input.clone(), response("world"))
            .expect("first script");

        assert!(matches!(
            provider
                .clone()
                .with_error(input.clone(), MockChatProviderError::Timeout),
            Err(MockConfigurationError::DuplicateRequest)
        ));
        assert_eq!(
            provider
                .complete(request("not scripted"))
                .await
                .unwrap_err(),
            MockChatProviderError::UnscriptedRequest
        );
    }

    #[tokio::test]
    async fn concurrent_identical_requests_do_not_consume_a_shared_queue() {
        let input = request("same input");
        let provider = Arc::new(
            MockChatProvider::new()
                .with_response(input.clone(), response("same output"))
                .expect("unique script"),
        );

        let tasks = (0..32).map(|_| {
            let provider = Arc::clone(&provider);
            let input = input.clone();
            tokio::spawn(async move { provider.complete(input).await })
        });

        for task in tasks {
            assert_eq!(
                task.await.expect("task completes").expect("scripted"),
                response("same output")
            );
        }
    }
}
