//! Provider-neutral validation for structured chat output.
//!
//! Capability negotiation happens before the provider request. Response
//! validation happens after the provider returns normalized content.

use std::fmt;

use qembed_domain::chat::{ChatRequest, ChatResponse, StructuredOutputMode};
use qembed_domain::ports::ChatProvider;

/// Failure while applying a structured-output contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructuredOutputError {
    /// No provider configuration exists for the requested routing key.
    UnconfiguredModel,
    /// The selected provider configuration cannot satisfy this output mode.
    UnsupportedMode(StructuredOutputMode),
    /// The provider returned content that is not exactly one valid JSON value.
    MalformedJson,
}

impl fmt::Display for StructuredOutputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnconfiguredModel => {
                formatter.write_str("no provider is configured for the model routing key")
            }
            Self::UnsupportedMode(mode) => {
                write!(
                    formatter,
                    "provider does not support structured output mode {mode:?}"
                )
            }
            Self::MalformedJson => {
                formatter.write_str("provider returned malformed JSON for JsonOnly output")
            }
        }
    }
}

impl std::error::Error for StructuredOutputError {}

/// Negotiation failure before a provider request is sent.
///
/// Reads the adapter's local capability profile for the request's routing
/// key. Call this before invoking ChatProvider::complete.
pub fn negotiate_request<P: ChatProvider>(
    provider: &P,
    request: &ChatRequest,
) -> Result<(), StructuredOutputError> {
    let capabilities = provider
        .capabilities(request.model_key())
        .ok_or(StructuredOutputError::UnconfiguredModel)?;
    if capabilities.supports(request.structured_output()) {
        Ok(())
    } else {
        Err(StructuredOutputError::UnsupportedMode(
            request.structured_output(),
        ))
    }
}

/// Rejects a mode that the selected provider configuration cannot honor.
///
/// None is always accepted because it imposes no output constraint.
/// supported_modes describes the provider adapter's configured support; this
/// function does not discover capabilities or infer them from a model name.
pub fn ensure_mode_supported(
    mode: StructuredOutputMode,
    supported_modes: &[StructuredOutputMode],
) -> Result<(), StructuredOutputError> {
    if mode == StructuredOutputMode::None || supported_modes.contains(&mode) {
        return Ok(());
    }

    Err(StructuredOutputError::UnsupportedMode(mode))
}

/// Validates normalized response content against the request's output mode.
///
/// JsonOnly accepts any single JSON value (object, array, scalar, or null)
/// and rejects surrounding prose, markdown fences, and malformed JSON.
/// None leaves the normalized non-empty text untouched.
pub fn validate_response(
    mode: StructuredOutputMode,
    response: ChatResponse,
) -> Result<ChatResponse, StructuredOutputError> {
    if mode == StructuredOutputMode::JsonOnly
        && serde_json::from_str::<serde_json::Value>(response.content()).is_err()
    {
        return Err(StructuredOutputError::MalformedJson);
    }

    Ok(response)
}

#[cfg(test)]
mod tests {
    use qembed_domain::chat::{ChatCapabilities, ChatFinishReason, ChatProviderError};

    use super::*;

    fn response(content: &str) -> ChatResponse {
        ChatResponse::new(content, ChatFinishReason::Stop, None).expect("valid response")
    }

    #[test]
    fn json_only_accepts_a_single_json_value() {
        let normalized = validate_response(
            StructuredOutputMode::JsonOnly,
            response(r#"{"answer":42,"ok":true}"#),
        )
        .expect("valid JSON response");

        assert_eq!(normalized.content(), r#"{"answer":42,"ok":true}"#);
    }

    #[test]
    fn json_only_rejects_malformed_json_and_surrounding_prose() {
        for content in [
            r#"{"answer": "#,
            "Here is the JSON: {\"answer\":42}",
            "~~~json\n{\"answer\":42}\n~~~",
            r#"{"answer":42} trailing text"#,
        ] {
            assert_eq!(
                validate_response(StructuredOutputMode::JsonOnly, response(content)),
                Err(StructuredOutputError::MalformedJson),
                "content should be rejected: {content}"
            );
        }
    }

    #[test]
    fn none_mode_preserves_plain_text_without_json_parsing() {
        let normalized = validate_response(
            StructuredOutputMode::None,
            response("ordinary prose, not JSON"),
        )
        .expect("unconstrained text is valid");

        assert_eq!(normalized.content(), "ordinary prose, not JSON");
    }

    #[test]
    fn mock_provider_exposes_distinct_configured_capability_profiles() {
        use crate::mock::MockChatProvider;

        let provider = MockChatProvider::new().with_capabilities(
            "route-json",
            ChatCapabilities::new([StructuredOutputMode::JsonOnly]),
        );
        let json_profile = provider
            .capabilities("route-json")
            .expect("configured route");
        let text_profile = provider
            .capabilities("test-chat")
            .expect("default test route");

        assert!(json_profile.supports(StructuredOutputMode::JsonOnly));
        assert!(!text_profile.supports(StructuredOutputMode::JsonOnly));
        assert!(text_profile.supports(StructuredOutputMode::None));
        assert!(provider.capabilities("unknown-route").is_none());
    }

    #[tokio::test]
    async fn mock_rejects_unsupported_capability_before_completion() {
        use crate::mock::MockChatProvider;
        use qembed_domain::chat::{ChatFinishReason, ChatMessage, ChatRole};

        let request = ChatRequest::new(
            "test-chat",
            vec![ChatMessage::new(ChatRole::User, "hello").expect("valid message")],
            16,
        )
        .expect("valid request")
        .with_structured_output(StructuredOutputMode::JsonOnly);
        let response =
            ChatResponse::new("{}", ChatFinishReason::Stop, None).expect("valid response");
        let provider = MockChatProvider::new()
            .with_response(request.clone(), response)
            .expect("unique fixture");

        assert_eq!(
            provider.complete(request).await.unwrap_err(),
            ChatProviderError::Other
        );
    }

    #[test]
    fn unsupported_mode_is_rejected_before_provider_request() {
        let request = qembed_domain::chat::ChatRequest::new(
            "test-chat",
            vec![
                qembed_domain::chat::ChatMessage::new(qembed_domain::chat::ChatRole::User, "hello")
                    .unwrap(),
            ],
            16,
        )
        .unwrap()
        .with_structured_output(StructuredOutputMode::JsonOnly);
        let plain_provider = crate::mock::MockChatProvider::new();
        assert_eq!(
            negotiate_request(&plain_provider, &request),
            Err(StructuredOutputError::UnsupportedMode(
                StructuredOutputMode::JsonOnly
            ))
        );
        let unknown_route = request.clone();
        let unconfigured_provider = crate::mock::MockChatProvider::new();
        let unconfigured_request = qembed_domain::chat::ChatRequest::new(
            "unknown-route",
            unknown_route.messages().to_vec(),
            unknown_route.max_output_tokens(),
        )
        .unwrap()
        .with_structured_output(StructuredOutputMode::JsonOnly);
        assert_eq!(
            negotiate_request(&unconfigured_provider, &unconfigured_request),
            Err(StructuredOutputError::UnconfiguredModel)
        );
        let json_provider = crate::mock::MockChatProvider::new().with_capabilities(
            "route-json",
            qembed_domain::chat::ChatCapabilities::new([StructuredOutputMode::JsonOnly]),
        );
        let json_request = qembed_domain::chat::ChatRequest::new(
            "route-json",
            request.messages().to_vec(),
            request.max_output_tokens(),
        )
        .unwrap()
        .with_structured_output(StructuredOutputMode::JsonOnly);
        assert_eq!(negotiate_request(&json_provider, &json_request), Ok(()));
        assert_eq!(
            ensure_mode_supported(
                StructuredOutputMode::JsonOnly,
                &[StructuredOutputMode::None],
            ),
            Err(StructuredOutputError::UnsupportedMode(
                StructuredOutputMode::JsonOnly
            ))
        );
        assert_eq!(
            ensure_mode_supported(StructuredOutputMode::None, &[]),
            Ok(())
        );
    }
}
