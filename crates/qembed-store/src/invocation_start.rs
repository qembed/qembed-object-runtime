//! PostgreSQL adapter for the atomic begin-invocation use case.

use crate::begin_invocation::{
    BeginInvocationCommand, BeginInvocationError, BeginInvocationOutcome, begin_invocation,
};
use crate::PgContext;
use qembed_domain::{
    ports::{InvocationStartCommand, InvocationStartOutcome, InvocationStartStore},
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
