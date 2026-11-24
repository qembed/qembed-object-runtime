//! Use case for public Anonymous invocation with fingerprint-scoped admission.

use std::time::{Duration, SystemTime};

use qembed_domain::{
    anonymous::AnonymousContext,
    ids::TraceId,
    jcs::{JcsError, canonicalize_json},
    ports::{
        AnonymousInvocationStartCommand, AnonymousInvocationStartStore,
        InvocationIdempotencyPreflight, InvocationStartOutcome,
    },
    rate_limit::{
        RateLimitKey, RateLimitPolicy, RateLimitRequestId, RateLimitRequestIdError, RateLimitStore,
    },
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    invocation_start::InvocationStartResult,
    rate_limit::{RateLimitService, RateLimitServiceError},
};

const IDEMPOTENCY_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const MESSAGE_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Clone, Debug)]
pub struct AnonymousInvocationStartRequest {
    pub idempotency_key: String,
    pub input: Value,
}

#[derive(Debug)]
pub enum AnonymousInvocationStartError<E> {
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

/// Starts a public invocation without accepting Principal or Caller identity
/// from the transport. PostgreSQL resolves the stable Object-scoped Caller;
/// the server fingerprint isolates rate admission and idempotency per subject.
#[derive(Clone, Debug)]
pub struct AnonymousInvocationStartService<S> {
    store: S,
}

impl<S: AnonymousInvocationStartStore> AnonymousInvocationStartService<S> {
    #[must_use]
    pub const fn new(store: S) -> Self {
        Self { store }
    }

    pub async fn start_rate_limited<R: RateLimitStore>(
        &self,
        context: &AnonymousContext,
        request: AnonymousInvocationStartRequest,
        rate_limit: &RateLimitService<R>,
        policy: RateLimitPolicy,
        cost: u32,
    ) -> Result<InvocationStartResult, AnonymousInvocationStartError<S::Error>> {
        let raw_idempotency_key = request.idempotency_key.clone();
        let request_id = RateLimitRequestId::for_anonymous_invocation(
            context.tenant_id(),
            context.object_id(),
            context.subject_fingerprint(),
            &raw_idempotency_key,
        )
        .map_err(AnonymousInvocationStartError::InvalidIdempotencyKey)?;
        let command = prepare_command(request, request_id.as_str().to_owned())?;

        let preflight = self
            .store
            .inspect_anonymous_invocation_idempotency(
                context,
                command.idempotency_key.clone(),
                command.payload_hash.clone(),
            )
            .await
            .map_err(AnonymousInvocationStartError::Store)?;

        let mut rate_limit_degraded = false;
        match preflight {
            InvocationIdempotencyPreflight::PayloadMismatch => {
                return Err(AnonymousInvocationStartError::IdempotencyConflict);
            }
            InvocationIdempotencyPreflight::ObjectNotFound => {
                return Err(AnonymousInvocationStartError::ObjectNotFound);
            }
            InvocationIdempotencyPreflight::ObjectNotActive => {
                return Err(AnonymousInvocationStartError::ObjectNotActive);
            }
            InvocationIdempotencyPreflight::NoPublishedVersion => {
                return Err(AnonymousInvocationStartError::NoPublishedVersion);
            }
            InvocationIdempotencyPreflight::NotPubliclyInvokable => {
                return Err(AnonymousInvocationStartError::NotPubliclyInvokable);
            }
            InvocationIdempotencyPreflight::Replay { .. } => {
                // begin_anonymous_invocation revalidates Object eligibility and
                // idempotency in the authoritative write transaction.
            }
            InvocationIdempotencyPreflight::New => {
                let key = RateLimitKey::for_anonymous(
                    context.tenant_id(),
                    context.object_id(),
                    context.subject_fingerprint(),
                )
                .map_err(|_| AnonymousInvocationStartError::RateLimitConfiguration)?;

                match rate_limit
                    .check_once(&key, policy, cost, &request_id, IDEMPOTENCY_RETENTION)
                    .await
                {
                    Ok(decision) if !decision.allowed() => {
                        return Err(AnonymousInvocationStartError::RateLimited {
                            retry_after: decision.retry_after(),
                        });
                    }
                    Ok(_) => {}
                    Err(RateLimitServiceError::Store(_)) => {
                        // Valkey is ephemeral admission control. Preserve
                        // availability, but surface fail-open to telemetry.
                        rate_limit_degraded = true;
                    }
                    Err(
                        RateLimitServiceError::InvalidCost
                        | RateLimitServiceError::InvalidMarkerTtl,
                    ) => {
                        return Err(AnonymousInvocationStartError::RateLimitConfiguration);
                    }
                }
            }
        }

        match self
            .store
            .begin_anonymous_invocation(context, command)
            .await
            .map_err(AnonymousInvocationStartError::Store)?
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
    request: AnonymousInvocationStartRequest,
    idempotency_key: String,
) -> Result<AnonymousInvocationStartCommand, AnonymousInvocationStartError<E>> {
    let input_bytes = serde_json::to_vec(&request.input)
        .map_err(|_| AnonymousInvocationStartError::InputSerialization)?;
    let canonical_input =
        canonicalize_json(&input_bytes).map_err(AnonymousInvocationStartError::InvalidInput)?;
    let input_message = String::from_utf8(canonical_input)
        .map_err(|_| AnonymousInvocationStartError::InputSerialization)?;

    let envelope = serde_json::json!({ "input": request.input });
    let envelope_bytes = serde_json::to_vec(&envelope)
        .map_err(|_| AnonymousInvocationStartError::InputSerialization)?;
    let canonical_envelope =
        canonicalize_json(&envelope_bytes).map_err(AnonymousInvocationStartError::InvalidInput)?;
    let payload_hash = Sha256::digest(canonical_envelope).to_vec();

    let now = SystemTime::now();
    let idempotency_expires_at = now
        .checked_add(IDEMPOTENCY_RETENTION)
        .ok_or(AnonymousInvocationStartError::RetentionTimeOutOfRange)?;
    let message_expires_at = now
        .checked_add(MESSAGE_RETENTION)
        .ok_or(AnonymousInvocationStartError::RetentionTimeOutOfRange)?;

    Ok(AnonymousInvocationStartCommand {
        idempotency_key,
        payload_hash,
        input_message,
        message_expires_at,
        idempotency_expires_at,
        trace_id: TraceId::new(),
    })
}
