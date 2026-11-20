//! Owner use case for starting a durable invocation.

use std::time::{Duration, SystemTime};

use qembed_domain::{
    caller::InvocationPool,
    ids::{CallerId, InvocationId, ObjectId, TraceId},
    jcs::{JcsError, canonicalize_json},
    ports::{InvocationStartCommand, InvocationStartOutcome, InvocationStartStore},
    scope::Scope,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

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
    Accepted { invocation_id: InvocationId },
    Replayed { invocation_id: InvocationId },
}

#[derive(Debug)]
pub enum InvocationStartError<E> {
    InvalidInput(JcsError),
    InputSerialization,
    RetentionTimeOutOfRange,
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

    /// Canonicalizes and hashes the input on the server, then commits a durable
    /// Received invocation. Provider/network work is intentionally not started here.
    pub async fn start(
        &self,
        scope: &Scope,
        request: InvocationStartRequest,
    ) -> Result<InvocationStartResult, InvocationStartError<S::Error>> {
        let input_bytes = serde_json::to_vec(&request.input)
            .map_err(|_| InvocationStartError::InputSerialization)?;
        let canonical_input = canonicalize_json(&input_bytes)
            .map_err(InvocationStartError::InvalidInput)?;
        let input_message = String::from_utf8(canonical_input)
            .map_err(|_| InvocationStartError::InputSerialization)?;

        let envelope = serde_json::json!({ "input": request.input });
        let envelope_bytes = serde_json::to_vec(&envelope)
            .map_err(|_| InvocationStartError::InputSerialization)?;
        let canonical_envelope = canonicalize_json(&envelope_bytes)
            .map_err(InvocationStartError::InvalidInput)?;
        let payload_hash = Sha256::digest(canonical_envelope).to_vec();

        let now = SystemTime::now();
        let idempotency_expires_at = now
            .checked_add(IDEMPOTENCY_RETENTION)
            .ok_or(InvocationStartError::RetentionTimeOutOfRange)?;
        let message_expires_at = now
            .checked_add(MESSAGE_RETENTION)
            .ok_or(InvocationStartError::RetentionTimeOutOfRange)?;

        let command = InvocationStartCommand {
            object_id: request.object_id,
            requested_caller_id: request.caller_id,
            invocation_pool: InvocationPool::Authenticated,
            idempotency_key: request.idempotency_key,
            payload_hash,
            input_message,
            message_expires_at,
            idempotency_expires_at,
            trace_id: TraceId::new(),
        };

        match self.store.begin_invocation(scope, command).await
            .map_err(InvocationStartError::Store)? {
            InvocationStartOutcome::Created { invocation_id } => {
                Ok(InvocationStartResult::Accepted { invocation_id })
            }
            InvocationStartOutcome::Replay { invocation_id } => {
                Ok(InvocationStartResult::Replayed { invocation_id })
            }
        }
    }
}
