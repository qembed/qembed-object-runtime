//! Stable, provider-neutral failures produced by runtime use cases.
//!
//! This taxonomy deliberately contains no SQLx, HTTP, or provider SDK types.
//! Adapter-specific failures must be classified before crossing into runtime.

use std::fmt;

use qembed_domain::chat::{ChatContractError, ChatProviderError};

/// Semantic runtime failure categories.
///
/// Variants describe the meaning of a failed operation, not its implementation
/// detail. The API layer owns the mapping from these categories to HTTP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeError {
    InvalidRequest,
    Unauthenticated,
    Forbidden,
    NotFound,
    Conflict,
    BudgetExceeded,
    RateLimited { retry_after_ms: Option<u64> },
    DependencyUnavailable,
    ProviderFailure,
    Internal,
}

impl From<ChatContractError> for RuntimeError {
    fn from(error: ChatContractError) -> Self {
        match error {
            ChatContractError::EmptyModelKey
            | ChatContractError::ModelKeyTooLong
            | ChatContractError::EmptyMessages
            | ChatContractError::TooManyMessages
            | ChatContractError::EmptyMessageContent
            | ChatContractError::MessageTooLarge
            | ChatContractError::RequestTooLarge
            | ChatContractError::InvalidOutputTokenLimit => Self::InvalidRequest,
            ChatContractError::EmptyResponse | ChatContractError::ResponseTooLarge => {
                Self::ProviderFailure
            }
        }
    }
}

impl From<ChatProviderError> for RuntimeError {
    fn from(error: ChatProviderError) -> Self {
        match error {
            ChatProviderError::Timeout | ChatProviderError::Unavailable => {
                Self::DependencyUnavailable
            }
            ChatProviderError::RateLimited { retry_after_ms } => {
                Self::RateLimited { retry_after_ms }
            }
            ChatProviderError::MalformedOutput | ChatProviderError::Other => Self::ProviderFailure,
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidRequest => "runtime rejected an invalid request",
            Self::Unauthenticated => "authentication is required",
            Self::Forbidden => "operation is not permitted",
            Self::NotFound => "resource was not found",
            Self::Conflict => "operation conflicts with current state",
            Self::BudgetExceeded => "configured usage budget was exceeded",
            Self::RateLimited { .. } => "upstream provider rate limited the request",
            Self::DependencyUnavailable => "a required dependency is unavailable",
            Self::ProviderFailure => "provider failed to satisfy the response contract",
            Self::Internal => "unexpected internal runtime failure",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for RuntimeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_contract_failures_and_provider_response_failures_are_distinct() {
        assert_eq!(
            RuntimeError::from(ChatContractError::EmptyMessages),
            RuntimeError::InvalidRequest
        );
        assert_eq!(
            RuntimeError::from(ChatContractError::InvalidOutputTokenLimit),
            RuntimeError::InvalidRequest
        );
        assert_eq!(
            RuntimeError::from(ChatContractError::EmptyResponse),
            RuntimeError::ProviderFailure
        );
        assert_eq!(
            RuntimeError::from(ChatContractError::ResponseTooLarge),
            RuntimeError::ProviderFailure
        );
    }

    #[test]
    fn provider_failures_are_classified_without_sdk_details() {
        assert_eq!(
            RuntimeError::from(ChatProviderError::Timeout),
            RuntimeError::DependencyUnavailable
        );
        assert_eq!(
            RuntimeError::from(ChatProviderError::Unavailable),
            RuntimeError::DependencyUnavailable
        );
        assert_eq!(
            RuntimeError::from(ChatProviderError::RateLimited {
                retry_after_ms: Some(1_500)
            }),
            RuntimeError::RateLimited {
                retry_after_ms: Some(1_500)
            }
        );
        assert_eq!(
            RuntimeError::from(ChatProviderError::MalformedOutput),
            RuntimeError::ProviderFailure
        );
        assert_eq!(
            RuntimeError::from(ChatProviderError::Other),
            RuntimeError::ProviderFailure
        );
    }
}
