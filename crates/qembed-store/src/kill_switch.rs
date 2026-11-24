//! PostgreSQL adapter for the canonical hierarchical kill-switch table.

use std::{fmt, str::FromStr};

use crate::{PgContext, fetch_all};
use qembed_domain::{
    ids::{ObjectId, TenantId, ToolId},
    kill_switch::{KillSwitch, KillSwitchContext, KillSwitchScope, KillSwitchStore},
    scope::Scope,
};

type KillSwitchRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
    Option<String>,
);

/// PostgreSQL implementation. The canonical `kill_switches` table and its
/// RLS/management rules remain owned by qembed-sql.
#[derive(Clone)]
pub struct PgKillSwitchStore {
    context: PgContext,
}

impl PgKillSwitchStore {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }
}

#[derive(Debug)]
pub enum KillSwitchStoreError {
    Context(crate::ContextTransactionError),
    Database(sqlx::Error),
    TenantMismatch,
    InvalidPersistedValue(&'static str),
}

impl fmt::Display for KillSwitchStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context(error) => write!(
                formatter,
                "kill-switch context initialization failed: {error}"
            ),
            Self::Database(error) => write!(formatter, "kill-switch query failed: {error}"),
            Self::TenantMismatch => {
                formatter.write_str("kill-switch context tenant differs from authenticated Scope")
            }
            Self::InvalidPersistedValue(field) => {
                write!(formatter, "persisted kill switch has invalid {field}")
            }
        }
    }
}

impl std::error::Error for KillSwitchStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::TenantMismatch | Self::InvalidPersistedValue(_) => None,
        }
    }
}

impl From<crate::ContextTransactionError> for KillSwitchStoreError {
    fn from(error: crate::ContextTransactionError) -> Self {
        Self::Context(error)
    }
}

impl From<sqlx::Error> for KillSwitchStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl KillSwitchStore for PgKillSwitchStore {
    type Error = KillSwitchStoreError;

    async fn load_matching_kill_switches(
        &self,
        scope: &Scope,
        context: KillSwitchContext,
    ) -> Result<Vec<KillSwitch>, Self::Error> {
        if scope.tenant_id() != context.tenant_id() {
            return Err(KillSwitchStoreError::TenantMismatch);
        }

        let mut transaction = self.context.begin_principal(scope).await?;
        let rows = fetch_all(
            &mut transaction,
            sqlx::query_as::<_, KillSwitchRow>(
                "SELECT scope_type, tenant_id::text, object_id::text, tool_id::text, enabled, reason \
                 FROM kill_switches \
                 WHERE scope_type = 'platform' \
                    OR (scope_type = 'tenant' AND tenant_id = $1::uuid) \
                    OR (scope_type = 'object' AND tenant_id = $1::uuid AND object_id = $2::uuid) \
                    OR (scope_type = 'tool' AND tool_id = $3::uuid AND (tenant_id IS NULL OR tenant_id = $1::uuid))",
            )
            .bind(context.tenant_id().to_string())
            .bind(context.object_id().to_string())
            .bind(context.tool_id().to_string()),
        )
        .await?;
        transaction.commit().await?;

        rows.into_iter().map(reconstitute_switch).collect()
    }
}

fn reconstitute_switch(row: KillSwitchRow) -> Result<KillSwitch, KillSwitchStoreError> {
    let (scope_type, tenant_id, object_id, tool_id, enabled, reason) = row;
    let tenant_id = parse_optional_id::<TenantId>(tenant_id, "tenant_id")?;
    let object_id = parse_optional_id::<ObjectId>(object_id, "object_id")?;
    let tool_id = parse_optional_id::<ToolId>(tool_id, "tool_id")?;
    let scope = match scope_type.as_str() {
        "platform" if tenant_id.is_none() && object_id.is_none() && tool_id.is_none() => {
            KillSwitchScope::Platform
        }
        "tenant" if tenant_id.is_some() && object_id.is_none() && tool_id.is_none() => {
            KillSwitchScope::Tenant {
                tenant_id: tenant_id.expect("checked above"),
            }
        }
        "object" if tenant_id.is_some() && object_id.is_some() && tool_id.is_none() => {
            KillSwitchScope::Object {
                tenant_id: tenant_id.expect("checked above"),
                object_id: object_id.expect("checked above"),
            }
        }
        "tool" if object_id.is_none() && tool_id.is_some() => KillSwitchScope::Tool {
            tenant_id,
            tool_id: tool_id.expect("checked above"),
        },
        _ => {
            return Err(KillSwitchStoreError::InvalidPersistedValue(
                "scope identity",
            ));
        }
    };
    Ok(KillSwitch::new(scope, enabled, reason))
}

fn parse_optional_id<T: FromStr>(
    value: Option<String>,
    field: &'static str,
) -> Result<Option<T>, KillSwitchStoreError> {
    value
        .map(|value| {
            value
                .parse()
                .map_err(|_| KillSwitchStoreError::InvalidPersistedValue(field))
        })
        .transpose()
}
