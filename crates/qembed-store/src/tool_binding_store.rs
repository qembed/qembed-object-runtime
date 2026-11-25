//! PostgreSQL adapter for tenant-scoped Object-to-Tool binding validation.
use std::{error::Error, fmt};

use qembed_domain::{
    ids::ObjectId, ports::ToolBindingStore, scope::Scope, tool_binding::ToolBindingIdentity,
};
use sqlx::Row;

use crate::{PgContext, fetch_optional};

#[derive(Debug)]
pub enum PgToolBindingStoreError {
    Context(crate::ContextTransactionError),
    Database(sqlx::Error),
    InvalidBindingVersion,
}

impl fmt::Display for PgToolBindingStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context(error) => {
                write!(f, "Tool binding context initialization failed: {error}")
            }
            Self::Database(error) => write!(f, "Tool binding lookup failed: {error}"),
            Self::InvalidBindingVersion => {
                f.write_str("persisted Tool binding version is invalid")
            }
        }
    }
}

impl Error for PgToolBindingStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::InvalidBindingVersion => None,
        }
    }
}

impl From<crate::ContextTransactionError> for PgToolBindingStoreError {
    fn from(value: crate::ContextTransactionError) -> Self {
        Self::Context(value)
    }
}

impl From<sqlx::Error> for PgToolBindingStoreError {
    fn from(value: sqlx::Error) -> Self {
        Self::Database(value)
    }
}

/// Read-only validation adapter. It never resolves a newer version as a fallback.
#[derive(Clone)]
pub struct PgToolBindingStore {
    context: PgContext,
}

impl PgToolBindingStore {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }
}

impl ToolBindingStore for PgToolBindingStore {
    type Error = PgToolBindingStoreError;

    async fn is_tool_binding_current(
        &self,
        scope: &Scope,
        object_id: ObjectId,
        binding: ToolBindingIdentity,
    ) -> Result<bool, Self::Error> {
        let mut transaction = self.context.begin_principal(scope).await?;
        let row = fetch_optional(
            &mut transaction,
            sqlx::query(
                "SELECT enabled FROM object_tools
                  WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND tool_id=$3::uuid
                    AND tool_version_id=$4::uuid AND binding_version=$5",
            )
            .bind(scope.tenant_id().to_string())
            .bind(object_id.to_string())
            .bind(binding.tool_id().to_string())
            .bind(binding.tool_version_id().to_string())
            .bind(
                i64::try_from(binding.binding_version())
                    .map_err(|_| PgToolBindingStoreError::InvalidBindingVersion)?,
            ),
        )
        .await?;

        let current = row
            .map(|row| row.try_get::<bool, _>("enabled"))
            .transpose()?
            .unwrap_or(false);
        transaction.commit().await?;
        Ok(current)
    }
}
