//! Provider-neutral chat request and response contracts.
//!
//! These types deliberately contain no SDK objects, HTTP details, vendor
//! model identifiers, or provider-specific option maps. The configured model
//! name is a logical routing key owned by the application.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

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
        }

        let raw = RawRequest::deserialize(deserializer)?;
        Self::new(raw.model_key, raw.messages, raw.max_output_tokens)
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
