//! Stable public API error contract and runtime-to-HTTP mapping.
//!
//! Public responses use fixed messages and codes. Internal error strings,
//! database diagnostics, provider payloads, and SDK metadata are never copied
//! into this response type.

use qembed_runtime::error::RuntimeError;
use qembed_store::begin_invocation::{BeginInvocationError, BudgetReservationError};
use qembed_store::caller::CallerResolutionError;
use qembed_store::idempotency::IdempotencyReservationError;
use qembed_store::invocation_query::InvocationQueryError;
use qembed_store::invocation_worker::InvocationStoreError;
use serde::Serialize;

/// Public error body. Both fields are stable client-facing contract values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ApiErrorBody {
    pub code: &'static str,
    pub message: &'static str,
}

/// HTTP status and safe response body for a failed API operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApiError {
    status: u16,
    body: ApiErrorBody,
}

impl ApiError {
    #[must_use]
    pub const fn status(self) -> u16 {
        self.status
    }

    #[must_use]
    pub const fn body(self) -> ApiErrorBody {
        self.body
    }
}

/// Maps a typed runtime failure to the stable public API contract.
///
/// Do not use Display/to_string() on the runtime or adapter error to construct
/// a response. The mapping intentionally emits only fixed, reviewed messages.
#[must_use]
pub const fn map_runtime_error(error: RuntimeError) -> ApiError {
    let (status, code, message) = match error {
        RuntimeError::InvalidRequest => (400, "invalid_request", "The request is invalid."),
        RuntimeError::Unauthenticated => (401, "unauthorized", "Authentication is required."),
        RuntimeError::Forbidden => (403, "forbidden", "The operation is not permitted."),
        RuntimeError::NotFound => (404, "not_found", "The requested resource was not found."),
        RuntimeError::Conflict => (
            409,
            "conflict",
            "The request conflicts with the current resource state.",
        ),
        RuntimeError::BudgetExceeded => (
            429,
            "budget_exceeded",
            "The configured usage budget has been exceeded.",
        ),
        RuntimeError::RateLimited { .. } => (
            429,
            "rate_limited",
            "The upstream service is rate limiting requests.",
        ),
        RuntimeError::DependencyUnavailable => (
            503,
            "dependency_unavailable",
            "A required dependency is temporarily unavailable.",
        ),
        RuntimeError::ProviderFailure => (
            502,
            "provider_error",
            "The upstream provider could not complete the request.",
        ),
        RuntimeError::Internal => (
            500,
            "internal_error",
            "An unexpected internal error occurred.",
        ),
    };

    ApiError {
        status,
        body: ApiErrorBody { code, message },
    }
}

/// Maps the current begin-invocation Store contract to safe public semantics.
///
/// Only positively identified domain outcomes become client errors. Database,
/// malformed persisted data, and inconsistent configuration fail closed.
#[must_use]
pub fn map_begin_invocation_error(error: &BeginInvocationError) -> ApiError {
    let runtime_error = match error {
        BeginInvocationError::CallerResolution(CallerResolutionError::NotDelegated)
        | BeginInvocationError::ObjectNotFound
        | BeginInvocationError::SnapshotNotFound => RuntimeError::NotFound,
        BeginInvocationError::Idempotency(IdempotencyReservationError::PayloadMismatch) => {
            RuntimeError::Conflict
        }
        BeginInvocationError::Idempotency(IdempotencyReservationError::ExpiryOutOfRange)
        | BeginInvocationError::InvalidInputMessage
        | BeginInvocationError::InvalidMessageExpiry => RuntimeError::InvalidRequest,
        BeginInvocationError::Budget(BudgetReservationError::Rejected(_)) => {
            RuntimeError::BudgetExceeded
        }
        BeginInvocationError::ObjectNotActive
        | BeginInvocationError::NoPublishedVersion
        | BeginInvocationError::SnapshotIsNotCurrentPublishedVersion => RuntimeError::Conflict,
        BeginInvocationError::CallerResolution(_)
        | BeginInvocationError::Idempotency(_)
        | BeginInvocationError::Budget(_)
        | BeginInvocationError::SnapshotBudgetInvalid
        | BeginInvocationError::KnowledgeRevisionNotFound
        | BeginInvocationError::KnowledgeRevisionOutOfRange
        | BeginInvocationError::Database(_)
        | BeginInvocationError::Context(_) => RuntimeError::Internal,
    };
    map_runtime_error(runtime_error)
}

/// Query errors indicate a persistence or persisted-data failure. A missing or
/// cross-tenant invocation is represented by Ok(None), not by this error type.
#[must_use]
pub fn map_invocation_query_error(_: &InvocationQueryError) -> ApiError {
    map_runtime_error(RuntimeError::Internal)
}

/// Maps invocation persistence failures without exposing database or state
/// diagnostics. The current Store error variant is too broad to distinguish a
/// client conflict from an internal invariant failure, so every variant fails
/// closed as internal_error until the Store contract provides narrower types.
#[must_use]
pub fn map_invocation_store_error(_: &InvocationStoreError) -> ApiError {
    map_runtime_error(RuntimeError::Internal)
}

/// Converts an unclassified adapter/store failure to a safe generic response.
///
/// This is a final fail-closed fallback. Known semantic failures should be
/// translated to RuntimeError at the trusted application boundary first.
#[must_use]
pub const fn map_unclassified_error<E: std::error::Error + ?Sized>(_: &E) -> ApiError {
    map_runtime_error(RuntimeError::Internal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qembed_domain::chat::{ChatContractError, ChatProviderError};
    use qembed_store::begin_invocation::{BeginInvocationError, BudgetRejection};

    #[test]
    fn every_runtime_category_has_a_stable_status_and_code() {
        let cases = [
            (RuntimeError::InvalidRequest, 400, "invalid_request"),
            (RuntimeError::Unauthenticated, 401, "unauthorized"),
            (RuntimeError::Forbidden, 403, "forbidden"),
            (RuntimeError::NotFound, 404, "not_found"),
            (RuntimeError::Conflict, 409, "conflict"),
            (RuntimeError::BudgetExceeded, 429, "budget_exceeded"),
            (
                RuntimeError::RateLimited {
                    retry_after_ms: Some(900),
                },
                429,
                "rate_limited",
            ),
            (
                RuntimeError::DependencyUnavailable,
                503,
                "dependency_unavailable",
            ),
            (RuntimeError::ProviderFailure, 502, "provider_error"),
            (RuntimeError::Internal, 500, "internal_error"),
        ];

        for (runtime_error, expected_status, expected_code) in cases {
            let mapped = map_runtime_error(runtime_error);
            assert_eq!(mapped.status(), expected_status);
            assert_eq!(mapped.body().code, expected_code);
        }
    }

    #[test]
    fn provider_and_contract_errors_map_to_fixed_public_messages() {
        let invalid = map_runtime_error(RuntimeError::from(ChatContractError::EmptyMessages));
        assert_eq!(invalid.body().code, "invalid_request");

        let provider_response =
            map_runtime_error(RuntimeError::from(ChatContractError::EmptyResponse));
        assert_eq!(provider_response.body().code, "provider_error");

        let provider = map_runtime_error(RuntimeError::from(ChatProviderError::Other));
        assert_eq!(provider.status(), 502);
        assert_eq!(provider.body().code, "provider_error");
        assert_eq!(
            provider.body().message,
            "The upstream provider could not complete the request."
        );
    }

    #[test]
    fn begin_invocation_semantic_failures_map_without_exposing_store_details() {
        let cases = [
            (BeginInvocationError::ObjectNotFound, 404, "not_found"),
            (
                BeginInvocationError::Idempotency(IdempotencyReservationError::PayloadMismatch),
                409,
                "conflict",
            ),
            (
                BeginInvocationError::Budget(BudgetReservationError::Rejected(
                    BudgetRejection::InvocationLimit,
                )),
                429,
                "budget_exceeded",
            ),
            (BeginInvocationError::ObjectNotActive, 409, "conflict"),
            (BeginInvocationError::NoPublishedVersion, 409, "conflict"),
            (
                BeginInvocationError::InvalidInputMessage,
                400,
                "invalid_request",
            ),
            (
                BeginInvocationError::SnapshotBudgetInvalid,
                500,
                "internal_error",
            ),
        ];

        for (store_error, expected_status, expected_code) in cases {
            let mapped = map_begin_invocation_error(&store_error);
            assert_eq!(mapped.status(), expected_status);
            assert_eq!(mapped.body().code, expected_code);
        }
    }

    #[test]
    fn query_and_invocation_store_failures_use_safe_fallbacks() {
        let query =
            map_invocation_query_error(&InvocationQueryError::InvalidPersistedValue("status"));
        assert_eq!(query.status(), 500);
        assert_eq!(query.body().code, "internal_error");

        let rejected_write = map_invocation_store_error(
            &InvocationStoreError::InvalidRequestedState("private state detail"),
        );
        assert_eq!(rejected_write.status(), 500);
        assert_eq!(rejected_write.body().code, "internal_error");

        let malformed_store =
            map_invocation_store_error(&InvocationStoreError::AmbiguousMessage("assistant"));
        assert_eq!(malformed_store.status(), 500);
        assert_eq!(malformed_store.body().code, "internal_error");
    }

    #[test]
    fn serialized_response_contains_no_internal_diagnostics() {
        let mapped = map_runtime_error(RuntimeError::Internal);
        let json = serde_json::to_value(mapped.body()).expect("body serializes");
        assert_eq!(
            json,
            serde_json::json!({
                "code": "internal_error",
                "message": "An unexpected internal error occurred."
            })
        );
        let serialized = json.to_string().to_lowercase();
        for forbidden in [
            "sqlx",
            "postgres",
            "provider_sdk",
            "connection refused",
            "select ",
        ] {
            assert!(!serialized.contains(forbidden));
        }
    }

    #[test]
    fn unclassified_store_or_adapter_errors_fail_closed() {
        #[derive(Debug)]
        struct SecretError;
        impl std::fmt::Display for SecretError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("postgres://secret-host/internal diagnostic")
            }
        }
        impl std::error::Error for SecretError {}

        let mapped = map_unclassified_error(&SecretError);
        assert_eq!(mapped.status(), 500);
        assert_eq!(mapped.body().code, "internal_error");
        assert!(!mapped.body().message.contains("secret-host"));
    }
}
