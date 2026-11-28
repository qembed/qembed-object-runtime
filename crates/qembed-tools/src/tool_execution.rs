//! Lease-fenced execution of Allow-authorized tool commands.
//!
//! The durable CommandId is also the upstream idempotency key. It is derived
//! from the invocation, step and action digest, so every worker/restart uses
//! the same key for the same logical side effect.

use std::{error::Error, fmt, future::Future, time::Duration};

use qembed_domain::{
    command_payload::CommandPayloadStore,
    ids::{CommandId, ToolExecutionId},
    policy::AllowedCommand,
    scope::Scope,
    tool_execution::{
        ToolExecution, ToolExecutionError, ToolExecutionOutcome, ToolExecutionStatus,
    },
    tool_execution_lease::{ToolExecutionCompletion, ToolExecutionLeaseStore},
    tool_version::ToolVersion,
};
use serde_json::Value;

use crate::{
    binding_validation::{
        BindingValidationError, validate_allowed_command_binding,
    },
    command_payload_encryption::{
        CommandPayloadWorkflowError, DataKeyProvider, PayloadCryptoError,
        load_authorized_command_payload,
    },
};

/// Request sent to a concrete upstream adapter.
///
/// Arguments are intentionally omitted from Debug output because they may
/// contain secrets or personal data.
pub struct UpstreamToolRequest {
    command_id: CommandId,
    idempotency_key: Option<String>,
    arguments: Value,
    timeout: Duration,
}

impl UpstreamToolRequest {
    #[must_use]
    pub const fn command_id(&self) -> CommandId {
        self.command_id
    }

    /// Present when the upstream adapter advertises idempotency-key support.
    #[must_use]
    pub fn idempotency_key(&self) -> Option<&str> {
        self.idempotency_key.as_deref()
    }

    #[must_use]
    pub const fn arguments(&self) -> &Value {
        &self.arguments
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl fmt::Debug for UpstreamToolRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpstreamToolRequest")
            .field("command_id", &self.command_id)
            .field("idempotency_key", &self.idempotency_key)
            .field("arguments", &"[REDACTED]")
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// A provider response that establishes a definitive outcome.
///
/// Adapters must return Failed only when the provider can establish that the
/// requested side effect did not succeed. Transport errors and timeouts are
/// represented by Err and are treated as ambiguous.
#[derive(Clone, Debug, PartialEq)]
pub enum UpstreamToolOutcome {
    Succeeded { output: Option<Value> },
    Failed,
}

/// Provider adapter for one immutable ToolVersion.
///
/// When supports_idempotency_keys is true, execute must propagate the supplied
/// key to the provider's documented idempotency mechanism. Repeated requests
/// with that key must represent one logical side effect.
pub trait ToolUpstream: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn supports_idempotency_keys(&self) -> bool;

    fn execute(
        &self,
        request: UpstreamToolRequest,
    ) -> impl Future<Output = Result<UpstreamToolOutcome, Self::Error>> + Send;
}

/// Durable result of one executor attempt.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolExecutionResult {
    /// A definitive upstream result was committed under the current lease.
    Completed(ToolExecution),
    /// The upstream outcome is ambiguous. The lease is left intact; expiry
    /// recovery will move it to Unknown and the reconciliation workflow decides.
    Ambiguous {
        command_id: CommandId,
        attempts: u32,
    },
    /// Another worker or a newer fencing token won the race.
    Stale,
}

/// Failure before or while preparing a tool execution.
#[derive(Debug)]
pub enum ToolExecutionWorkflowError<StoreError, BindingError, PayloadError> {
    Store(StoreError),
    Binding(BindingValidationError<BindingError>),
    PayloadStore(PayloadError),
    PayloadCrypto(PayloadCryptoError),
    NotFound,
    NotAuthorized(ToolExecutionStatus),
    PayloadMissing,
    InvalidIdentity,
    ToolVersionMismatch,
    InvalidLeaseOwner,
    InvalidLeaseDuration,
    InvalidAttemptCount,
    InvalidOutputTtl,
    InvalidArguments,
    Domain(ToolExecutionError),
}

impl<S, B, P> fmt::Display for ToolExecutionWorkflowError<S, B, P>
where
    S: fmt::Display,
    B: fmt::Display,
    P: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "ToolExecution store failed: {error}"),
            Self::Binding(error) => write!(formatter, "Tool binding validation failed: {error}"),
            Self::PayloadStore(error) => write!(formatter, "command payload store failed: {error}"),
            Self::PayloadCrypto(error) => write!(formatter, "command payload verification failed: {error}"),
            Self::NotFound => formatter.write_str("ToolExecution was not found"),
            Self::NotAuthorized(status) => write!(formatter, "ToolExecution is not Authorized: {status:?}"),
            Self::PayloadMissing => formatter.write_str("encrypted command payload is missing or expired"),
            Self::InvalidIdentity => formatter.write_str("AllowedCommand does not identify this ToolExecution"),
            Self::ToolVersionMismatch => formatter.write_str("ToolVersion does not match the authorized binding"),
            Self::InvalidLeaseOwner => formatter.write_str("tool worker lease owner must be non-empty"),
            Self::InvalidLeaseDuration => formatter.write_str("tool worker lease must exceed the upstream timeout"),
            Self::InvalidAttemptCount => formatter.write_str("maximum attempt count must be positive"),
            Self::InvalidOutputTtl => formatter.write_str("output retention TTL must be positive"),
            Self::InvalidArguments => formatter.write_str("authorized command arguments are not valid JSON"),
            Self::Domain(error) => write!(formatter, "ToolExecution state transition failed: {error:?}"),
        }
    }
}

/// Executes one persisted ToolExecution after revalidating its binding.
///
/// A transport error/timeout is never converted to Failed. It is retried only
/// when the immutable ToolVersion is idempotent AND the adapter guarantees
/// upstream idempotency-key handling. Every retry uses the same CommandId key.
/// Non-idempotent actions are attempted at most once.
#[allow(clippy::too_many_arguments)]
pub async fn execute_tool_command<S, B, P, K, U>(
    execution_store: &S,
    binding_store: &B,
    payload_store: &P,
    key_provider: &K,
    upstream: &U,
    scope: &Scope,
    execution_id: ToolExecutionId,
    command: AllowedCommand,
    tool_version: &ToolVersion,
    worker: &str,
    lease_seconds: u32,
    max_attempts: u32,
    output_ttl_seconds: u32,
) -> Result<
    ToolExecutionResult,
    ToolExecutionWorkflowError<S::Error, B::Error, P::Error>,
>
where
    S: ToolExecutionLeaseStore,
    B: qembed_domain::ports::ToolBindingStore,
    P: CommandPayloadStore,
    K: DataKeyProvider,
    U: ToolUpstream,
{
    if worker.trim().is_empty() {
        return Err(ToolExecutionWorkflowError::InvalidLeaseOwner);
    }
    if max_attempts == 0 {
        return Err(ToolExecutionWorkflowError::InvalidAttemptCount);
    }
    if output_ttl_seconds == 0 {
        return Err(ToolExecutionWorkflowError::InvalidOutputTtl);
    }
    let timeout_ms = tool_version.timeout_ms();
    let minimum_lease = timeout_ms.div_ceil(1_000).saturating_add(1);
    if lease_seconds < minimum_lease {
        return Err(ToolExecutionWorkflowError::InvalidLeaseDuration);
    }

    let validated = validate_allowed_command_binding(binding_store, scope, command)
        .await
        .map_err(ToolExecutionWorkflowError::Binding)?;
    let allowed = validated.command();
    let binding = allowed
        .tool_binding()
        .ok_or(ToolExecutionWorkflowError::InvalidIdentity)?;

    if binding.tool_id() != tool_version.tool_id()
        || binding.tool_version_id() != tool_version.id()
    {
        return Err(ToolExecutionWorkflowError::ToolVersionMismatch);
    }

    let persisted = execution_store
        .load_tool_execution(scope, execution_id)
        .await
        .map_err(ToolExecutionWorkflowError::Store)?
        .ok_or(ToolExecutionWorkflowError::NotFound)?;

    if persisted.tenant_id() != scope.tenant_id()
        || persisted.object_id() != allowed.object_id()
        || persisted.invocation_id() != allowed.invocation_id()
        || persisted.command_id() != allowed.command_id()
    {
        return Err(ToolExecutionWorkflowError::InvalidIdentity);
    }
    if persisted.status() != ToolExecutionStatus::Authorized {
        return Err(ToolExecutionWorkflowError::NotAuthorized(persisted.status()));
    }

    // Decrypt and verify the immutable Allow digest before acquiring a lease or
    // making a network call. Missing/expired payloads therefore cause no side effect.
    let plaintext = match load_authorized_command_payload(
        payload_store,
        key_provider,
        scope,
        allowed.command_id(),
        *allowed.action_hash(),
    )
    .await
    {
        Ok(Some(plaintext)) => plaintext,
        Ok(None) => return Err(ToolExecutionWorkflowError::PayloadMissing),
        Err(CommandPayloadWorkflowError::Store(error)) => {
            return Err(ToolExecutionWorkflowError::PayloadStore(error));
        }
        Err(CommandPayloadWorkflowError::Crypto(error)) => {
            return Err(ToolExecutionWorkflowError::PayloadCrypto(error));
        }
    };
    let arguments: Value = serde_json::from_slice(&plaintext)
        .map_err(|_| ToolExecutionWorkflowError::InvalidArguments)?;

    // The SQL claim repeats the binding check in its atomic UPDATE, closing the
    // race between the read-only binding validation above and lease acquisition.
    let Some(mut claimed) = execution_store
        .claim_tool_execution(scope, execution_id, worker, lease_seconds)
        .await
        .map_err(ToolExecutionWorkflowError::Store)?
    else {
        return Ok(ToolExecutionResult::Stale);
    };

    let command_id = allowed.command_id();
    let stable_key = format!("qembed-command:{command_id}");
    let upstream_supports_keys = upstream.supports_idempotency_keys();
    let attempts_allowed = if tool_version.idempotent() && upstream_supports_keys {
        max_attempts
    } else {
        1
    };
    let timeout = Duration::from_millis(u64::from(timeout_ms));

    for attempt in 1..=attempts_allowed {
        // Renew before each network attempt. A stale worker must not initiate
        // another side effect after losing its ToolExecution lease.
        let lease_live = execution_store
            .heartbeat_tool_execution(
                scope,
                execution_id,
                worker,
                claimed.version(),
                claimed.fencing_token(),
                lease_seconds,
            )
            .await
            .map_err(ToolExecutionWorkflowError::Store)?;
        if !lease_live {
            return Ok(ToolExecutionResult::Stale);
        }

        let request = UpstreamToolRequest {
            command_id,
            idempotency_key: upstream_supports_keys.then(|| stable_key.clone()),
            arguments: arguments.clone(),
            timeout,
        };
        let call = tokio::time::timeout(timeout, upstream.execute(request)).await;

        let definitive = match call {
            Ok(Ok(UpstreamToolOutcome::Succeeded { output })) => Some((true, output)),
            Ok(Ok(UpstreamToolOutcome::Failed)) => Some((false, None)),
            Ok(Err(_)) | Err(_) => None,
        };

        if let Some((succeeded, output)) = definitive {
            let outcome = if succeeded {
                ToolExecutionOutcome::Succeeded
            } else {
                ToolExecutionOutcome::Failed
            };
            claimed
                .complete(worker, claimed.fencing_token(), outcome)
                .map_err(ToolExecutionWorkflowError::Domain)?;
            let completion = ToolExecutionCompletion {
                execution_id,
                owner: worker.to_owned(),
                expected_version: claimed.version() - 1,
                expected_fencing_token: claimed.fencing_token(),
                next: claimed.clone(),
                output_ttl_seconds: output.as_ref().map(|_| output_ttl_seconds),
                output,
            };
            let committed = execution_store
                .complete_tool_execution(scope, completion)
                .await
                .map_err(ToolExecutionWorkflowError::Store)?;
            return Ok(if committed {
                ToolExecutionResult::Completed(claimed)
            } else {
                ToolExecutionResult::Stale
            });
        }

        if attempt == attempts_allowed {
            return Ok(ToolExecutionResult::Ambiguous {
                command_id,
                attempts: attempt,
            });
        }
    }

    // attempts_allowed is positive, so every loop path returns above.
    unreachable!("tool execution attempt loop must return a result")
}

/// Returns the deterministic upstream key for a durable command identity.
#[must_use]
pub fn upstream_idempotency_key(command_id: CommandId) -> String {
    format!("qembed-command:{command_id}")
}

#[cfg(test)]
mod tests {
    use qembed_domain::ids::CommandId;

    use super::upstream_idempotency_key;

    #[test]
    fn upstream_idempotency_key_survives_reconstructing_command_id_after_restart() {
        let original = CommandId::new();
        let persisted = original.to_string();
        let restored: CommandId = persisted.parse().expect("persisted CommandId parses");

        assert_eq!(
            upstream_idempotency_key(original),
            upstream_idempotency_key(restored)
        );
    }
}
