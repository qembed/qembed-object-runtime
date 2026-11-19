//! Stable, provider-neutral failures produced by runtime use cases.
//!
//! This taxonomy deliberately contains no SQLx, HTTP, or provider SDK types.
//! Adapter-specific failures must be classified before crossing into runtime.

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
    fn from(_: ChatContractError) -> Self {
        Self::InvalidRequest
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
            ChatProviderError::MalformedOutput | ChatProviderError::Other => {
                Self::ProviderFailure
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_contract_failures_are_client_input_errors() {
        assert_eq!(
            RuntimeError::from(ChatContractError::EmptyMessages),
            RuntimeError::InvalidRequest
        );
        assert_eq!(
            RuntimeError::from(ChatContractError::ResponseTooLarge),
            RuntimeError::InvalidRequest
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
