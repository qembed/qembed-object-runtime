//! Provider-neutral chat request and response contracts.
//!
//! These types deliberately contain no SDK objects, HTTP details, vendor
//! model identifiers, or provider-specific option maps. The configured model
//! name is a logical routing key owned by the application.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::ids::InvocationId;

/// Maximum number of messages accepted in one chat request.
pub const MAX_CHAT_MESSAGES: usize = 128;
/// Maximum UTF-8 byte length of one message body.
pub const MAX_CHAT_MESSAGE_BYTES: usize = 64 * 1024;
/// Maximum combined UTF-8 byte length of all request message bodies.
pub const MAX_CHAT_REQUEST_BYTES: usize = 256 * 1024;
/// Maximum length of an application-configured model routing key.
pub const MAX_CHAT_MODEL_KEY_BYTES: usize = 128;
/// Maximum output token budget accepted by the domain contract.
pub const MAX_CHAT_OUTPUT_TOKENS: u32 = 32_768;
/// Maximum UTF-8 byte length of a normalized provider response.
pub const MAX_CHAT_RESPONSE_BYTES: usize = 1024 * 1024;

/// Validation failure for a provider-neutral chat contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatContractError {
    EmptyModelKey,
    ModelKeyTooLong,
    EmptyMessages,
    TooManyMessages,
    EmptyMessageContent,
    MessageTooLarge,
    RequestTooLarge,
    InvalidOutputTokenLimit,
    EmptyResponse,
    ResponseTooLarge,
}

impl fmt::Display for ChatContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyModelKey => "model routing key must not be empty",
            Self::ModelKeyTooLong => "model routing key exceeds the maximum length",
            Self::EmptyMessages => "chat request must contain at least one message",
            Self::TooManyMessages => "chat request contains too many messages",
            Self::EmptyMessageContent => "chat message content must not be empty",
            Self::MessageTooLarge => "chat message content exceeds the maximum length",
            Self::RequestTooLarge => "chat request content exceeds the maximum length",
            Self::InvalidOutputTokenLimit => "output token limit is outside the supported range",
            Self::EmptyResponse => "chat response content must not be empty",
            Self::ResponseTooLarge => "chat response content exceeds the maximum length",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ChatContractError {}

/// Semantic role of a message in a chat conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

/// Requested shape of a provider response.
///
/// This is a provider-neutral contract. `JsonOnly` requires the normalized
/// response content to contain exactly one valid JSON value; `None` leaves
/// response content unconstrained plain text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutputMode {
    /// No structured-output constraint; any non-empty text response is valid.
    #[default]
    None,
    /// The response content must be valid JSON without surrounding prose.
    JsonOnly,
}

/// Stable context for one logical provider operation.
///
/// The key is derived from the durable invocation ID, so every worker attempt
/// for the same invocation reuses it. It is deliberately not caller supplied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRequestContext {
    idempotency_key: String,
}

impl ProviderRequestContext {
    /// Creates the stable key for the invocation's primary chat completion.
    #[must_use]
    pub fn for_invocation(invocation_id: InvocationId) -> Self {
        Self {
            idempotency_key: format!("qembed:invocation:{invocation_id}:chat:v1"),
        }
    }

    /// Returns the opaque key an adapter may forward when the provider supports it.
    #[must_use]
    pub fn idempotency_key(&self) -> &str {
        &self.idempotency_key
    }
}

/// Explicit capabilities advertised by one configured provider/model route.
///
/// This descriptor contains only provider-neutral features. It is configuration
/// supplied by the adapter; it is never inferred from a model name or a
/// successful/failed completion request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChatCapabilities {
    structured_output_modes: Vec<StructuredOutputMode>,
    idempotency_keys: bool,
}

impl ChatCapabilities {
    /// Creates a capability profile. Unconstrained text mode is always
    /// supported; duplicate modes are removed.
    #[must_use]
    pub fn new(modes: impl IntoIterator<Item = StructuredOutputMode>) -> Self {
        let mut structured_output_modes = vec![StructuredOutputMode::None];
        for mode in modes {
            if !structured_output_modes.contains(&mode) {
                structured_output_modes.push(mode);
            }
        }
        Self {
            structured_output_modes,
            idempotency_keys: false,
        }
    }

    /// Returns a profile that advertises provider-side idempotency-key support.
    ///
    /// An adapter may set this only when it forwards the supplied request key
    /// to the provider and the provider guarantees deduplication for that key.
    #[must_use]
    pub const fn with_idempotency_keys(mut self) -> Self {
        self.idempotency_keys = true;
        self
    }

    /// Whether this route guarantees provider-side deduplication by request key.
    #[must_use]
    pub const fn supports_idempotency_keys(&self) -> bool {
        self.idempotency_keys
    }

    /// Returns the modes explicitly present in this normalized profile.
    #[must_use]
    pub fn structured_output_modes(&self) -> &[StructuredOutputMode] {
        &self.structured_output_modes
    }

    /// Whether this profile can satisfy the requested output mode.
    #[must_use]
    pub fn supports(&self, mode: StructuredOutputMode) -> bool {
        mode == StructuredOutputMode::None || self.structured_output_modes.contains(&mode)
    }
}

impl Default for ChatCapabilities {
    fn default() -> Self {
        Self::new([])
    }
}

/// One provider-neutral message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChatMessage {
    role: ChatRole,
    content: String,
}

impl ChatMessage {
    /// Creates a message after enforcing the per-message content limit.
    pub fn new(role: ChatRole, content: impl Into<String>) -> Result<Self, ChatContractError> {
        let content = content.into();
        if content.trim().is_empty() {
            return Err(ChatContractError::EmptyMessageContent);
        }
        if content.len() > MAX_CHAT_MESSAGE_BYTES {
            return Err(ChatContractError::MessageTooLarge);
        }
        Ok(Self { role, content })
    }

    #[must_use]
    pub const fn role(&self) -> ChatRole {
        self.role
    }

    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }
}

impl<'de> Deserialize<'de> for ChatMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawMessage {
            role: ChatRole,
            content: String,
        }

        let raw = RawMessage::deserialize(deserializer)?;
        Self::new(raw.role, raw.content).map_err(serde::de::Error::custom)
    }
}

/// Provider-neutral chat request.
///
/// `model_key` is an application configuration alias, not a provider model
/// name. Provider adapters resolve it using configuration outside this type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChatRequest {
    model_key: String,
    messages: Vec<ChatMessage>,
    max_output_tokens: u32,
    structured_output: StructuredOutputMode,
}

impl ChatRequest {
    /// Builds a request and validates all size and limit constraints.
    pub fn new(
        model_key: impl Into<String>,
        messages: Vec<ChatMessage>,
        max_output_tokens: u32,
    ) -> Result<Self, ChatContractError> {
        let model_key = model_key.into();
        if model_key.trim().is_empty() {
            return Err(ChatContractError::EmptyModelKey);
        }
        if model_key.len() > MAX_CHAT_MODEL_KEY_BYTES {
            return Err(ChatContractError::ModelKeyTooLong);
        }
        if messages.is_empty() {
            return Err(ChatContractError::EmptyMessages);
        }
        if messages.len() > MAX_CHAT_MESSAGES {
            return Err(ChatContractError::TooManyMessages);
        }
        if !(1..=MAX_CHAT_OUTPUT_TOKENS).contains(&max_output_tokens) {
            return Err(ChatContractError::InvalidOutputTokenLimit);
        }
        let total_bytes = messages
            .iter()
            .map(|message| message.content.len())
            .sum::<usize>();
        if total_bytes > MAX_CHAT_REQUEST_BYTES {
            return Err(ChatContractError::RequestTooLarge);
        }

        Ok(Self {
            model_key,
            messages,
            max_output_tokens,
            structured_output: StructuredOutputMode::None,
        })
    }

    #[must_use]
    pub fn model_key(&self) -> &str {
        &self.model_key
    }

    #[must_use]
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }

    /// Returns a copy with the requested provider-neutral output constraint.
    #[must_use]
    pub const fn with_structured_output(mut self, mode: StructuredOutputMode) -> Self {
        self.structured_output = mode;
        self
    }

    #[must_use]
    pub const fn structured_output(&self) -> StructuredOutputMode {
        self.structured_output
    }
}

impl<'de> Deserialize<'de> for ChatRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawRequest {
            model_key: String,
            messages: Vec<ChatMessage>,
            max_output_tokens: u32,
            #[serde(default)]
            structured_output: StructuredOutputMode,
        }

        let raw = RawRequest::deserialize(deserializer)?;
        Self::new(raw.model_key, raw.messages, raw.max_output_tokens)
            .map(|request| request.with_structured_output(raw.structured_output))
            .map_err(serde::de::Error::custom)
    }
}

/// Why the provider stopped generating a response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatFinishReason {
    Stop,
    Length,
    ContentFiltered,
}

/// Provider-neutral token accounting.
///
/// Values are optional at the response boundary because some providers do not
/// return complete usage data. Mapping and policy for missing usage belong to
/// the adapter/runtime boundary (RUST-034).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenUsage {
    input_tokens: u32,
    output_tokens: u32,
}

impl TokenUsage {
    #[must_use]
    pub const fn new(input_tokens: u32, output_tokens: u32) -> Self {
        Self {
            input_tokens,
            output_tokens,
        }
    }

    #[must_use]
    pub const fn input_tokens(self) -> u32 {
        self.input_tokens
    }

    #[must_use]
    pub const fn output_tokens(self) -> u32 {
        self.output_tokens
    }
}

/// Normalized chat response returned by a provider adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChatResponse {
    content: String,
    finish_reason: ChatFinishReason,
    usage: Option<TokenUsage>,
}

impl ChatResponse {
    /// Creates a normalized response and enforces the response size limit.
    pub fn new(
        content: impl Into<String>,
        finish_reason: ChatFinishReason,
        usage: Option<TokenUsage>,
    ) -> Result<Self, ChatContractError> {
        let content = content.into();
        if content.trim().is_empty() {
            return Err(ChatContractError::EmptyResponse);
        }
        if content.len() > MAX_CHAT_RESPONSE_BYTES {
            return Err(ChatContractError::ResponseTooLarge);
        }
        Ok(Self {
            content,
            finish_reason,
            usage,
        })
    }

    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    #[must_use]
    pub const fn finish_reason(&self) -> ChatFinishReason {
        self.finish_reason
    }

    #[must_use]
    pub const fn usage(&self) -> Option<TokenUsage> {
        self.usage
    }
}

impl<'de> Deserialize<'de> for ChatResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawResponse {
            content: String,
            finish_reason: ChatFinishReason,
            usage: Option<TokenUsage>,
        }

        let raw = RawResponse::deserialize(deserializer)?;
        Self::new(raw.content, raw.finish_reason, raw.usage).map_err(serde::de::Error::custom)
    }
}

/// Provider-neutral failure category returned by a chat provider adapter.
///
/// These categories deliberately omit SDK types, raw provider messages,
/// request IDs, and vendor-specific status codes. They describe what the
/// runtime may reason about, not whether or when it should retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatProviderError {
    /// The provider did not complete the request before its deadline.
    Timeout,
    /// The provider rejected the request because of rate limiting.
    RateLimited { retry_after_ms: Option<u64> },
    /// The provider or its upstream service was unavailable.
    Unavailable,
    /// The provider returned output that could not satisfy the response contract.
    MalformedOutput,
    /// A failure that does not fit a more specific provider-neutral category.
    Other,
}

impl ChatProviderError {
    /// Returns the provider's optional rate-limit delay hint in milliseconds.
    ///
    /// This is advisory metadata only; it does not prescribe retry behavior.
    #[must_use]
    pub const fn retry_after_ms(self) -> Option<u64> {
        match self {
            Self::RateLimited { retry_after_ms } => retry_after_ms,
            _ => None,
        }
    }
}

impl fmt::Display for ChatProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Timeout => "chat provider request timed out",
            Self::RateLimited { .. } => "chat provider rate limit exceeded",
            Self::Unavailable => "chat provider is unavailable",
            Self::MalformedOutput => "chat provider returned malformed output",
            Self::Other => "chat provider request failed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ChatProviderError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_message(content: &str) -> ChatMessage {
        ChatMessage::new(ChatRole::User, content).expect("valid user message")
    }

    #[test]
    fn request_serialization_round_trips_provider_neutral_fields() {
        let request = ChatRequest::new(
            "default-chat",
            vec![
                ChatMessage::new(ChatRole::System, "Be concise").expect("valid system message"),
                user_message("Hello"),
            ],
            512,
        )
        .expect("valid request");

        let json = serde_json::to_string(&request).expect("request should serialize");
        let decoded: ChatRequest = serde_json::from_str(&json).expect("request should deserialize");
        assert_eq!(decoded, request);
        assert!(json.contains("\"model_key\":\"default-chat\""));
        assert!(!json.contains("openai"));
    }

    #[test]
    fn structured_output_mode_defaults_to_none_and_round_trips() {
        let default = ChatRequest::new("default-chat", vec![user_message("hello")], 100)
            .expect("valid request");
        assert_eq!(default.structured_output(), StructuredOutputMode::None);

        let json_only = default
            .clone()
            .with_structured_output(StructuredOutputMode::JsonOnly);
        assert_ne!(default, json_only);
        let serialized = serde_json::to_string(&json_only).expect("request serializes");
        assert!(serialized.contains("\"structured_output\":\"json_only\""));
        let decoded: ChatRequest = serde_json::from_str(&serialized).expect("request deserializes");
        assert_eq!(decoded, json_only);
        assert_eq!(decoded.structured_output(), StructuredOutputMode::JsonOnly);
    }

    #[test]
    fn legacy_request_json_without_output_mode_defaults_to_none() {
        let json = r#"{"model_key":"default-chat","messages":[{"role":"user","content":"hello"}],"max_output_tokens":100}"#;
        let decoded: ChatRequest = serde_json::from_str(json).expect("legacy request is valid");
        assert_eq!(decoded.structured_output(), StructuredOutputMode::None);
    }

    #[test]
    fn response_serialization_round_trips_normalized_usage() {
        let response = ChatResponse::new(
            "Hello back",
            ChatFinishReason::Stop,
            Some(TokenUsage::new(12, 7)),
        )
        .expect("valid response");

        let json = serde_json::to_string(&response).expect("response should serialize");
        let decoded: ChatResponse =
            serde_json::from_str(&json).expect("response should deserialize");
        assert_eq!(decoded, response);
        assert_eq!(
            decoded.usage().expect("usage is present").input_tokens(),
            12
        );
    }

    #[test]
    fn request_rejects_missing_messages_and_invalid_limits() {
        assert_eq!(
            ChatRequest::new("default-chat", vec![], 100).unwrap_err(),
            ChatContractError::EmptyMessages
        );
        assert_eq!(
            ChatRequest::new("default-chat", vec![user_message("Hi")], 0).unwrap_err(),
            ChatContractError::InvalidOutputTokenLimit
        );
        assert_eq!(
            ChatRequest::new(
                "default-chat",
                vec![user_message("Hi")],
                MAX_CHAT_OUTPUT_TOKENS + 1,
            )
            .unwrap_err(),
            ChatContractError::InvalidOutputTokenLimit
        );
    }

    #[test]
    fn request_enforces_message_count_and_total_content_limits() {
        let too_many = (0..=MAX_CHAT_MESSAGES).map(|_| user_message("x")).collect();
        assert_eq!(
            ChatRequest::new("default-chat", too_many, 100).unwrap_err(),
            ChatContractError::TooManyMessages
        );

        assert_eq!(
            ChatMessage::new(ChatRole::User, "x".repeat(MAX_CHAT_MESSAGE_BYTES + 1)).unwrap_err(),
            ChatContractError::MessageTooLarge
        );
        let total_oversized = (0..5)
            .map(|index| {
                user_message(&format!(
                    "{index}{}",
                    "z".repeat(MAX_CHAT_MESSAGE_BYTES - 1)
                ))
            })
            .collect();
        assert_eq!(
            ChatRequest::new("default-chat", total_oversized, 100).unwrap_err(),
            ChatContractError::RequestTooLarge
        );
    }

    #[test]
    fn deserialization_rejects_missing_required_fields_and_unknown_extensions() {
        let valid_message = r#""messages":[{"role":"user","content":"hello"}]"#;
        let missing_model = format!(r#"{{{valid_message},"max_output_tokens":10}}"#);
        let missing_messages = r#"{"model_key":"default-chat","max_output_tokens":10}"#;
        let missing_limit = format!(r#"{{"model_key":"default-chat",{valid_message}}}"#);
        let unknown_extension = format!(
            r#"{{"model_key":"default-chat",{valid_message},"max_output_tokens":10,"vendor_options":{{}}}}"#
        );

        assert!(serde_json::from_str::<ChatRequest>(&missing_model).is_err());
        assert!(serde_json::from_str::<ChatRequest>(missing_messages).is_err());
        assert!(serde_json::from_str::<ChatRequest>(&missing_limit).is_err());
        assert!(serde_json::from_str::<ChatRequest>(&unknown_extension).is_err());

        assert!(serde_json::from_str::<ChatMessage>(r#"{"content":"hello"}"#).is_err());
        assert!(
            serde_json::from_str::<ChatResponse>(r#"{"finish_reason":"stop","usage":null}"#)
                .is_err()
        );
    }

    #[test]
    fn deserialization_cannot_bypass_contract_limits() {
        let json = format!(
            r#"{{"model_key":"default-chat","messages":[{{"role":"user","content":"{}"}}],"max_output_tokens":100}}"#,
            "x".repeat(MAX_CHAT_MESSAGE_BYTES + 1)
        );
        assert!(serde_json::from_str::<ChatRequest>(&json).is_err());
    }

    #[test]
    fn response_rejects_empty_and_oversized_content() {
        assert_eq!(
            ChatResponse::new("  ", ChatFinishReason::Stop, None).unwrap_err(),
            ChatContractError::EmptyResponse
        );
        assert_eq!(
            ChatResponse::new(
                "x".repeat(MAX_CHAT_RESPONSE_BYTES + 1),
                ChatFinishReason::Stop,
                None,
            )
            .unwrap_err(),
            ChatContractError::ResponseTooLarge
        );
    }
}
