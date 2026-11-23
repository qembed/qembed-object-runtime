//! PostgreSQL adapter for the atomic begin-invocation use case.

use crate::{PgContext, fetch_optional};
use crate::begin_invocation::{
    BeginInvocationCommand, BeginInvocationError, BeginInvocationOutcome, begin_invocation,
};
use qembed_domain::{
    ids::{CallerId, InvocationId, ObjectId},
    ports::{
        InvocationIdempotencyPreflight, InvocationStartCommand, InvocationStartOutcome,
        InvocationStartStore,
    },
    scope::Scope,
};

#[derive(Clone)]
pub struct PgInvocationStartStore {
    context: PgContext,
}

impl PgInvocationStartStore {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }
}

impl InvocationStartStore for PgInvocationStartStore {
    type Error = BeginInvocationError;

    async fn inspect_invocation_idempotency(
        &self,
        scope: &Scope,
        object_id: ObjectId,
        caller_id: CallerId,
        idempotency_key: String,
        payload_hash: Vec<u8>,
    ) -> Result<InvocationIdempotencyPreflight, Self::Error> {
        let (mut transaction, caller) = self.context.begin_caller(scope, caller_id).await?;
        let existing = fetch_optional(
            &mut transaction,
            sqlx::query_as::<_, (Vec<u8>, String)>(
                "SELECT payload_hash, invocation_id::text FROM invocation_idempotency WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND caller_id = $3::uuid AND idempotency_key = $4 FOR SHARE",
            )
            .bind(scope.tenant_id().to_string())
            .bind(object_id.to_string())
            .bind(caller.caller_id().to_string())
            .bind(idempotency_key),
        )
        .await
        .map_err(BeginInvocationError::Database)?;

        let result = match existing {
            Some((existing_hash, _)) if existing_hash != payload_hash => {
                InvocationIdempotencyPreflight::PayloadMismatch
            }
            Some((_, invocation_id)) => InvocationIdempotencyPreflight::Replay {
                invocation_id: invocation_id
                    .parse::<InvocationId>()
                    .map_err(|_| BeginInvocationError::Idempotency(
                        crate::idempotency::IdempotencyReservationError::InvalidPersistedInvocationId,
                    ))?,
            },
            None => InvocationIdempotencyPreflight::New,
        };

        transaction
            .commit()
            .await
            .map_err(BeginInvocationError::Database)?;
        Ok(result)
    }

    async fn begin_invocation(
        &self,
        scope: &Scope,
        command: InvocationStartCommand,
    ) -> Result<InvocationStartOutcome, Self::Error> {
        let command = BeginInvocationCommand {
            object_id: command.object_id,
            requested_caller_id: command.requested_caller_id,
            invocation_pool: command.invocation_pool,
            idempotency_key: command.idempotency_key,
            payload_hash: command.payload_hash,
            input_message: command.input_message,
            message_expires_at: command.message_expires_at,
            idempotency_expires_at: command.idempotency_expires_at,
            trace_id: command.trace_id,
        };
        match begin_invocation(&self.context, scope, command).await? {
            BeginInvocationOutcome::Created { invocation_id, .. } => {
                Ok(InvocationStartOutcome::Created { invocation_id })
            }
            BeginInvocationOutcome::Replay { invocation_id } => {
                Ok(InvocationStartOutcome::Replay { invocation_id })
            }
        }
    }
}
