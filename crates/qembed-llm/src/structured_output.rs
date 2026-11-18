//! Provider-neutral validation for structured chat output.
//!
//! Capability checks happen before the provider request. Response validation
//! happens after the provider returns normalized content. RUST-033 may replace
//! the explicit supported-mode slice with a richer capability descriptor.

use std::fmt;

use qembed_domain::chat::{ChatCapabilities, ChatRequest, ChatResponse, StructuredOutputMode};

/// Failure while applying a structured-output contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructuredOutputError {
    /// The selected provider configuration cannot satisfy this output mode.
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
            Self::UnconfiguredModel => formatter.write_str("no provider is configured for the model routing key"),
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
/// The caller obtains the capability profile from the adapter's local
/// configuration, then calls this function before invoking ChatProvider.
pub fn negotiate_request(
    request: &ChatRequest,
    capabilities: Option<&ChatCapabilities>,
) -> Result<(), StructuredOutputError> {
    let capabilities = capabilities.ok_or(StructuredOutputError::UnconfiguredModel)?;
    if capabilities.supports(request.structured_output()) {
        Ok(())
    } else {
        Err(StructuredOutputError::UnsupportedMode(request.structured_output()))
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
    use qembed_domain::chat::ChatFinishReason;

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
    fn unsupported_mode_is_rejected_before_provider_request() {
        let request = qembed_domain::chat::ChatRequest::new(
            "route-json",
            vec![qembed_domain::chat::ChatMessage::new(qembed_domain::chat::ChatRole::User, "hello").unwrap()],
            16,
        ).unwrap().with_structured_output(StructuredOutputMode::JsonOnly);
        let plain_profile = ChatCapabilities::default();
        assert_eq!(
            negotiate_request(&request, Some(&plain_profile)),
            Err(StructuredOutputError::UnsupportedMode(StructuredOutputMode::JsonOnly))
        );
        assert_eq!(negotiate_request(&request, None), Err(StructuredOutputError::UnconfiguredModel));
        let json_profile = ChatCapabilities::new([StructuredOutputMode::JsonOnly]);
        assert_eq!(negotiate_request(&request, Some(&json_profile)), Ok(()));
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
