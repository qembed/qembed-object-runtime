//! Tenant-scoped Owner management of Principal-to-Caller delegations.

use qembed_domain::{
    ids::{CallerId, DelegationId, PrincipalId},
    principal::PrincipalKind,
    scope::Scope,
};

use crate::PgContext;

/// PostgreSQL adapter for Owner-controlled Caller delegation lifecycle.
#[derive(Clone, Debug)]
pub struct PgCallerDelegationRepository {
    context: PgContext,
}

impl PgCallerDelegationRepository {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }

    /// Grants an active Caller to an active Principal in the Owner's tenant.
    /// PostgreSQL rechecks Owner authority and both target lifecycles under row locks.
    pub async fn grant(
        &self,
        scope: &Scope,
        target_principal_id: PrincipalId,
        caller_id: CallerId,
    ) -> Result<DelegationId, CallerDelegationError> {
        if scope.principal().kind() != PrincipalKind::TenantOwner {
            return Err(CallerDelegationError::NotOwner);
        }

        let mut transaction = self
            .context
            .begin_principal(scope)
            .await
            .map_err(CallerDelegationError::Context)?;
        let delegation_id: String = sqlx::query_scalar(
            "SELECT grant_caller_delegation($1::uuid, $2::uuid)::text",
        )
        .bind(target_principal_id.to_string())
        .bind(caller_id.to_string())
        .fetch_one(transaction.connection())
        .await
        .map_err(CallerDelegationError::Database)?;

        transaction
            .commit()
            .await
            .map_err(CallerDelegationError::Database)?;
        delegation_id
            .parse()
            .map_err(|_| CallerDelegationError::InvalidDelegationId)
    }

    /// Revokes a delegation without deleting its immutable history.
    pub async fn revoke(
        &self,
        scope: &Scope,
        delegation_id: DelegationId,
    ) -> Result<(), CallerDelegationError> {
        if scope.principal().kind() != PrincipalKind::TenantOwner {
            return Err(CallerDelegationError::NotOwner);
        }

        let mut transaction = self
            .context
            .begin_principal(scope)
            .await
            .map_err(CallerDelegationError::Context)?;
        let revoked: bool = sqlx::query_scalar(
            "SELECT revoke_caller_delegation($1::uuid)",
        )
        .bind(delegation_id.to_string())
        .fetch_one(transaction.connection())
        .await
        .map_err(CallerDelegationError::Database)?;

        transaction
            .commit()
            .await
            .map_err(CallerDelegationError::Database)?;
        if revoked {
            Ok(())
        } else {
            Err(CallerDelegationError::NotFound)
        }
    }
}

/// Failure while changing a Caller delegation.
#[derive(Debug)]
pub enum CallerDelegationError {
    NotOwner,
    NotFound,
    InvalidDelegationId,
    Context(crate::ContextTransactionError),
    Database(sqlx::Error),
}

impl std::fmt::Display for CallerDelegationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOwner => formatter.write_str("only a TenantOwner may manage Caller delegations"),
            Self::NotFound => formatter.write_str("active Caller delegation was not found"),
            Self::InvalidDelegationId => formatter.write_str("database returned an invalid delegation ID"),
            Self::Context(error) => write!(formatter, "delegation context error: {error}"),
            Self::Database(error) => write!(formatter, "Caller delegation database error: {error}"),
        }
    }
}

impl std::error::Error for CallerDelegationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}
