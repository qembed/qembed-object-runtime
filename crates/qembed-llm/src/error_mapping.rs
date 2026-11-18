//! Mapping boundary for provider-specific failures.
//!
//! Each concrete adapter implements `ProviderErrorClassifier` for its SDK
//! error type. Only the resulting provider-neutral category crosses into the
//! domain/runtime layer; raw SDK messages and payloads remain adapter-local.

use qembed_domain::chat::ChatProviderError;

/// Provider-neutral classification supplied by a concrete adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Timeout,
    RateLimited,
    Unavailable,
    MalformedOutput,
    Other,
}

/// Adapter-side view of an SDK error.
///
/// Implement this for the concrete SDK error type. Do not expose the SDK error
/// itself through domain or runtime APIs.
pub trait ProviderErrorClassifier {
    /// Returns the semantic category of this failure.
    fn kind(&self) -> ProviderErrorKind;

    /// Returns an optional rate-limit delay hint in milliseconds.
    ///
    /// The default is appropriate when the provider does not expose a reliable
    /// hint. This value is informational and does not trigger a retry.
    fn retry_after_ms(&self) -> Option<u64> {
        None
    }
}

/// Converts an adapter-specific error into the stable domain taxonomy.
#[must_use]
pub fn map_provider_error<E: ProviderErrorClassifier + ?Sized>(error: &E) -> ChatProviderError {
    match error.kind() {
        ProviderErrorKind::Timeout => ChatProviderError::Timeout,
        ProviderErrorKind::RateLimited => ChatProviderError::RateLimited {
            retry_after_ms: error.retry_after_ms(),
        },
        ProviderErrorKind::Unavailable => ChatProviderError::Unavailable,
        ProviderErrorKind::MalformedOutput => ChatProviderError::MalformedOutput,
        ProviderErrorKind::Other => ChatProviderError::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    struct FakeSdkError {
        kind: ProviderErrorKind,
        retry_after_ms: Option<u64>,
    }

    impl ProviderErrorClassifier for FakeSdkError {
        fn kind(&self) -> ProviderErrorKind {
            self.kind
        }

        fn retry_after_ms(&self) -> Option<u64> {
            self.retry_after_ms
        }
    }

    #[test]
    fn maps_every_provider_neutral_error_category() {
        let cases = [
            (ProviderErrorKind::Timeout, ChatProviderError::Timeout),
            (
                ProviderErrorKind::RateLimited,
                ChatProviderError::RateLimited {
                    retry_after_ms: None,
                },
            ),
            (
                ProviderErrorKind::Unavailable,
                ChatProviderError::Unavailable,
            ),
            (
                ProviderErrorKind::MalformedOutput,
                ChatProviderError::MalformedOutput,
            ),
            (ProviderErrorKind::Other, ChatProviderError::Other),
        ];

        for (kind, expected) in cases {
            assert_eq!(
                map_provider_error(&FakeSdkError {
                    kind,
                    retry_after_ms: None,
                }),
                expected
            );
        }
    }

    #[test]
    fn preserves_rate_limit_hint_without_assigning_retry_policy() {
        let mapped = map_provider_error(&FakeSdkError {
            kind: ProviderErrorKind::RateLimited,
            retry_after_ms: Some(1_500),
        });

        assert_eq!(
            mapped,
            ChatProviderError::RateLimited {
                retry_after_ms: Some(1_500)
            }
        );
        assert_eq!(mapped.retry_after_ms(), Some(1_500));
    }

    #[test]
    fn ignores_retry_hints_for_non_rate_limit_categories() {
        let mapped = map_provider_error(&FakeSdkError {
            kind: ProviderErrorKind::Unavailable,
            retry_after_ms: Some(1_500),
        });

        assert_eq!(mapped, ChatProviderError::Unavailable);
        assert_eq!(mapped.retry_after_ms(), None);
    }
}
