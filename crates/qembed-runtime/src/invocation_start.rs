//! Use cases for starting durable invocations with replay-aware admission.

use std::time::{Duration, SystemTime};

use qembed_domain::{
    caller::InvocationPool,
    ids::{CallerId, InvocationId, ObjectId, TraceId},
    jcs::{JcsError, canonicalize_json},
    ports::{
        InvocationIdempotencyPreflight, InvocationStartCommand, InvocationStartOutcome,
        InvocationStartStore,
    },
    rate_limit::{
        RateLimitKey, RateLimitPolicy, RateLimitRequestId, RateLimitRequestIdError, RateLimitStore,
    },
    scope::Scope,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::rate_limit::{RateLimitService, RateLimitServiceError};

const IDEMPOTENCY_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const MESSAGE_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Clone, Debug)]
pub struct InvocationStartRequest {
    pub object_id: ObjectId,
    pub caller_id: CallerId,
    pub idempotency_key: String,
    pub input: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvocationStartResult {
    Accepted {
        invocation_id: InvocationId,
        /// True means the Valkey adapter failed and admission proceeded under
        /// the documented fail-open policy. The API composition root must emit
        /// structured telemetry for this state.
        rate_limit_degraded: bool,
    },
    Replayed {
        invocation_id: InvocationId,
    },
}

#[derive(Debug)]
pub enum InvocationStartError<E> {
    InvalidInput(JcsError),
    InputSerialization,
    RetentionTimeOutOfRange,
    InvalidIdempotencyKey(RateLimitRequestIdError),
    IdempotencyConflict,
    ObjectNotFound,
    ObjectNotActive,
    NoPublishedVersion,
    NotPubliclyInvokable,
    RateLimited { retry_after: Option<Duration> },
    RateLimitConfiguration,
    Store(E),
}

#[derive(Clone, Debug)]
pub struct InvocationStartService<S> {
    store: S,
}

impl<S: InvocationStartStore> InvocationStartService<S> {
    #[must_use]
    pub const fn new(store: S) -> Self {
        Self { store }
    }

    /// Starts an invocation without distributed rate admission. Retained for
    /// internal/test callers; public API composition should use start_rate_limited.
    pub async fn start(
        &self,
        scope: &Scope,
        request: InvocationStartRequest,
    ) -> Result<InvocationStartResult, InvocationStartError<S::Error>> {
        let command = prepare_command(request)?;
        self.commit(scope, command, false).await
    }

    /// Authorizes the Caller and checks committed idempotency before touching
    /// Valkey. New requests atomically consume one bucket token and set a
    /// tenant/object-scoped request marker in the same Lua script. Concurrent
    /// retries with the same key therefore charge once, while PostgreSQL's
    /// begin-invocation transaction remains the authority for replay and payload
    /// mismatch. Caller authorization is revalidated by begin_invocation.
    pub async fn start_rate_limited<R: RateLimitStore>(
        &self,
        scope: &Scope,
        request: InvocationStartRequest,
        rate_limit: &RateLimitService<R>,
        rate_limit_key: &RateLimitKey,
        policy: RateLimitPolicy,
        cost: u32,
    ) -> Result<InvocationStartResult, InvocationStartError<S::Error>> {
        let object_id = request.object_id;
        let caller_id = request.caller_id;
        let idempotency_key = request.idempotency_key.clone();
        let command = prepare_command(request)?;

        let preflight = self
            .store
            .inspect_invocation_idempotency(
                scope,
                object_id,
                caller_id,
                idempotency_key.clone(),
                command.payload_hash.clone(),
            )
            .await
            .map_err(InvocationStartError::Store)?;

        let mut rate_limit_degraded = false;
        match preflight {
            InvocationIdempotencyPreflight::PayloadMismatch => {
                return Err(InvocationStartError::IdempotencyConflict);
            }
            InvocationIdempotencyPreflight::ObjectNotFound => {
                return Err(InvocationStartError::ObjectNotFound);
            }
            InvocationIdempotencyPreflight::ObjectNotActive => {
                return Err(InvocationStartError::ObjectNotActive);
            }
            InvocationIdempotencyPreflight::NoPublishedVersion => {
                return Err(InvocationStartError::NoPublishedVersion);
            }
            InvocationIdempotencyPreflight::NotPubliclyInvokable => {
                return Err(InvocationStartError::NotPubliclyInvokable);
            }
            InvocationIdempotencyPreflight::Replay { .. } => {
                // Still call begin_invocation below: it revalidates Caller
                // authorization and returns the authoritative replay result.
            }
            InvocationIdempotencyPreflight::New => {
                let request_id = RateLimitRequestId::for_invocation(
                    scope.tenant_id(),
                    object_id,
                    caller_id,
                    &idempotency_key,
                )
                .map_err(InvocationStartError::InvalidIdempotencyKey)?;

                match rate_limit
                    .check_once(
                        rate_limit_key,
                        policy,
                        cost,
                        &request_id,
                        IDEMPOTENCY_RETENTION,
                    )
                    .await
                {
                    Ok(decision) if !decision.allowed() => {
                        return Err(InvocationStartError::RateLimited {
                            retry_after: decision.retry_after(),
                        });
                    }
                    Ok(_) => {}
                    Err(RateLimitServiceError::Store(_)) => {
                        // Valkey is ephemeral admission control, not durable
                        // accounting. Preserve availability and report degraded
                        // admission to the composition root for telemetry.
                        rate_limit_degraded = true;
                    }
                    Err(
                        RateLimitServiceError::InvalidCost
                        | RateLimitServiceError::InvalidMarkerTtl,
                    ) => return Err(InvocationStartError::RateLimitConfiguration),
                }
            }
        }

        self.commit(scope, command, rate_limit_degraded).await
    }

    async fn commit(
        &self,
        scope: &Scope,
        command: InvocationStartCommand,
        rate_limit_degraded: bool,
    ) -> Result<InvocationStartResult, InvocationStartError<S::Error>> {
        match self
            .store
            .begin_invocation(scope, command)
            .await
            .map_err(InvocationStartError::Store)?
        {
            InvocationStartOutcome::Created { invocation_id } => {
                Ok(InvocationStartResult::Accepted {
                    invocation_id,
                    rate_limit_degraded,
                })
            }
            InvocationStartOutcome::Replay { invocation_id } => {
                Ok(InvocationStartResult::Replayed { invocation_id })
            }
        }
    }
}

fn prepare_command<E>(
    request: InvocationStartRequest,
) -> Result<InvocationStartCommand, InvocationStartError<E>> {
    let input_bytes =
        serde_json::to_vec(&request.input).map_err(|_| InvocationStartError::InputSerialization)?;
    let canonical_input =
        canonicalize_json(&input_bytes).map_err(InvocationStartError::InvalidInput)?;
    let input_message =
        String::from_utf8(canonical_input).map_err(|_| InvocationStartError::InputSerialization)?;

    let envelope = serde_json::json!({ "input": request.input });
    let envelope_bytes =
        serde_json::to_vec(&envelope).map_err(|_| InvocationStartError::InputSerialization)?;
    let canonical_envelope =
        canonicalize_json(&envelope_bytes).map_err(InvocationStartError::InvalidInput)?;
    let payload_hash = Sha256::digest(canonical_envelope).to_vec();

    let now = SystemTime::now();
    let idempotency_expires_at = now
        .checked_add(IDEMPOTENCY_RETENTION)
        .ok_or(InvocationStartError::RetentionTimeOutOfRange)?;
    let message_expires_at = now
        .checked_add(MESSAGE_RETENTION)
        .ok_or(InvocationStartError::RetentionTimeOutOfRange)?;

    Ok(InvocationStartCommand {
        object_id: request.object_id,
        requested_caller_id: request.caller_id,
        invocation_pool: InvocationPool::Authenticated,
        idempotency_key: request.idempotency_key,
        payload_hash,
        input_message,
        message_expires_at,
        idempotency_expires_at,
        trace_id: TraceId::new(),
    })
}
