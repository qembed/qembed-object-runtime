//! PostgreSQL adapter for encrypted, Allow-bound command payloads.

use crate::{PgContext, execute, fetch_optional};
use qembed_domain::{
    command_payload::{CommandPayloadStore, EncryptedCommandPayload},
    ids::CommandId,
    scope::Scope,
};
use std::fmt;

type PayloadRow = (Vec<u8>, String, Vec<u8>);

#[derive(Clone)]
pub struct PgCommandPayloadStore {
    context: PgContext,
}

impl PgCommandPayloadStore {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }
}

#[derive(Debug)]
pub enum CommandPayloadStoreError {
    Context(crate::ContextTransactionError),
    Database(sqlx::Error),
    InvalidRequest(&'static str),
}

impl fmt::Display for CommandPayloadStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context(error) => write!(
                f,
                "command payload signed-context initialization failed: {error}"
            ),
            Self::Database(error) => write!(f, "command payload persistence failed: {error}"),
            Self::InvalidRequest(error) => {
                write!(f, "invalid command payload request: {error}")
            }
        }
    }
}

impl std::error::Error for CommandPayloadStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::InvalidRequest(_) => None,
        }
    }
}

impl From<crate::ContextTransactionError> for CommandPayloadStoreError {
    fn from(error: crate::ContextTransactionError) -> Self {
        Self::Context(error)
    }
}

impl From<sqlx::Error> for CommandPayloadStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl CommandPayloadStore for PgCommandPayloadStore {
    type Error = CommandPayloadStoreError;

    async fn store_command_payload(
        &self,
        scope: &Scope,
        command_id: CommandId,
        payload: EncryptedCommandPayload,
        ttl_seconds: u32,
    ) -> Result<(), Self::Error> {
        if ttl_seconds == 0 {
            return Err(CommandPayloadStoreError::InvalidRequest(
                "payload TTL must be positive",
            ));
        }
        if payload.key_id().is_empty() {
            return Err(CommandPayloadStoreError::InvalidRequest(
                "key ID must not be empty",
            ));
        }

        let mut transaction = self.context.begin_principal(scope).await?;
        let result = execute(
            &mut transaction,
            sqlx::query(
                "INSERT INTO tool_command_payloads
                    (tenant_id, command_id, payload_ciphertext, key_id, arguments_hash, expires_at)
                 SELECT $1::uuid, $2::uuid, $3, $4, decision.arguments_hash,
                        clock_timestamp() + make_interval(secs => $5::double precision)
                   FROM policy_decisions AS decision
                  WHERE decision.tenant_id=$1::uuid AND decision.command_id=$2::uuid
                    AND decision.decision='Allow' AND decision.arguments_hash=$6
                 ON CONFLICT (tenant_id, command_id) DO UPDATE
                    SET payload_ciphertext=EXCLUDED.payload_ciphertext, key_id=EXCLUDED.key_id
                  WHERE tool_command_payloads.arguments_hash=EXCLUDED.arguments_hash",
            )
            .bind(scope.tenant_id().to_string())
            .bind(command_id.to_string())
            .bind(payload.ciphertext())
            .bind(payload.key_id())
            .bind(f64::from(ttl_seconds))
            .bind(payload.arguments_hash().as_slice()),
        )
        .await?;
        transaction.commit().await?;

        if result.rows_affected() != 1 {
            return Err(CommandPayloadStoreError::InvalidRequest(
                "command is not backed by a matching Allow decision",
            ));
        }
        Ok(())
    }

    async fn load_command_payload(
        &self,
        scope: &Scope,
        command_id: CommandId,
        expected_arguments_hash: [u8; 32],
    ) -> Result<Option<EncryptedCommandPayload>, Self::Error> {
        let mut transaction = self.context.begin_principal(scope).await?;
        let row = fetch_optional(
            &mut transaction,
            sqlx::query_as::<_, PayloadRow>(
                "SELECT payload.payload_ciphertext, payload.key_id, payload.arguments_hash
                   FROM tool_command_payloads AS payload
                   JOIN policy_decisions AS decision
                     ON decision.tenant_id=payload.tenant_id
                    AND decision.command_id=payload.command_id
                  WHERE payload.tenant_id=$1::uuid AND payload.command_id=$2::uuid
                    AND decision.decision='Allow'
                    AND decision.arguments_hash=$3
                    AND payload.arguments_hash=decision.arguments_hash
                    AND payload.expires_at > clock_timestamp()",
            )
            .bind(scope.tenant_id().to_string())
            .bind(command_id.to_string())
            .bind(expected_arguments_hash.as_slice()),
        )
        .await?;
        transaction.commit().await?;

        row.map(|(ciphertext, key_id, hash)| {
            let hash: [u8; 32] = hash.try_into().map_err(|_| {
                CommandPayloadStoreError::InvalidRequest(
                    "persisted arguments hash is not 32 bytes",
                )
            })?;
            Ok(EncryptedCommandPayload::new(ciphertext, key_id, hash))
        })
        .transpose()
    }
}
