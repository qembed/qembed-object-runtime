//! PostgreSQL-backed resolution of an explicitly selected invocation Caller.

use std::fmt;

use qembed_domain::caller::CallerKind;
use qembed_domain::ids::{CallerId, PrincipalId, TenantId};

use crate::{TenantTransaction, TransactionToken, fetch_optional};

/// A Caller proven to be actively delegated to the authenticated Principal.
///
/// Fields are private and the only constructor is the database resolver. This
/// value can be passed to caller-scoped persistence primitives without
/// accidentally substituting the authenticated Principal ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedCaller {
    caller_id: CallerId,
    kind: CallerKind,
    tenant_id: TenantId,
    principal_id: Option<PrincipalId>,
    transaction_token: TransactionToken,
}

impl ResolvedCaller {
    pub(crate) fn anonymous(caller_id: CallerId, transaction: &TenantTransaction) -> Self {
        Self {
            caller_id,
            kind: CallerKind::Anonymous,
            tenant_id: transaction.tenant_id(),
            principal_id: None,
            transaction_token: transaction.transaction_token(),
        }
    }

    /// Returns the effective invocation identity.
    #[must_use]
    pub const fn caller_id(self) -> CallerId {
        self.caller_id
    }

    /// Returns the persisted Caller kind.
    #[must_use]
    pub const fn kind(self) -> CallerKind {
        self.kind
    }

    pub(crate) fn is_bound_to(
        &self,
        transaction: &TenantTransaction,
        scope: &qembed_domain::scope::Scope,
    ) -> bool {
        self.tenant_id == scope.tenant_id()
            && self.principal_id == Some(scope.principal().principal_id())
            && self.transaction_token == transaction.transaction_token()
            && transaction.matches_scope(scope)
    }

    pub(crate) fn is_bound_to_transaction(&self, transaction: &TenantTransaction) -> bool {
        self.tenant_id == transaction.tenant_id()
            && self.principal_id == transaction.principal_id()
            && self.transaction_token == transaction.transaction_token()
    }
}

/// Resolves the caller requested by the application against the authenticated
/// Principal's active tenant-scoped delegation.
///
/// The SQL function locks the delegation, Principal, and Caller rows FOR SHARE.
/// Keep this transaction open until the invocation has been inserted and
/// committed; the locks serialize begin-invocation against concurrent revocation
/// or identity disablement.
pub async fn resolve_delegated_caller(
    transaction: &mut TenantTransaction,
    requested_caller_id: CallerId,
) -> Result<ResolvedCaller, CallerResolutionError> {
    let principal_id = transaction
        .principal_id()
        .ok_or(CallerResolutionError::PrincipalContextRequired)?;
    let row = fetch_optional(
        transaction,
        sqlx::query_as::<_, (String, String)>(
            "SELECT caller_id::text, caller_kind \
             FROM resolve_delegated_caller($1::uuid, $2::uuid, $3::uuid)",
        )
        .bind(transaction.tenant_id().to_string())
        .bind(principal_id.to_string())
        .bind(requested_caller_id.to_string()),
    )
    .await
    .map_err(|error| match &error {
        sqlx::Error::Database(database_error)
            if database_error.code().as_deref() == Some("42501") =>
        {
            CallerResolutionError::NotDelegated
        }
        _ => CallerResolutionError::Database(error),
    })?
    .ok_or(CallerResolutionError::NotDelegated)?;

    let caller_id = row
        .0
        .parse()
        .map_err(|_| CallerResolutionError::InvalidCallerId)?;
    let kind = CallerKind::parse(&row.1).ok_or(CallerResolutionError::UnknownCallerKind)?;

    Ok(ResolvedCaller {
        caller_id,
        kind,
        tenant_id: transaction.tenant_id(),
        principal_id: Some(principal_id),
        transaction_token: transaction.transaction_token(),
    })
}

/// Failure to resolve an effective Caller.
#[derive(Debug)]
pub enum CallerResolutionError {
    /// The requested Caller is not actively delegated to this Principal.
    NotDelegated,
    /// The transaction has no authenticated Principal context.
    PrincipalContextRequired,
    /// The Object is not enabled for Anonymous invocation or its Caller is inactive.
    AnonymousNotAllowed,
    /// The database returned a malformed Caller identifier.
    InvalidCallerId,
    /// The database returned a Caller kind unknown to this Rust version.
    UnknownCallerKind,
    /// PostgreSQL could not complete the resolution.
    Database(sqlx::Error),
}

impl fmt::Display for CallerResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotDelegated => {
                formatter.write_str("Caller is not actively delegated to this Principal")
            }
            Self::PrincipalContextRequired => {
                formatter.write_str("delegated Caller resolution requires a Principal context")
            }
            Self::AnonymousNotAllowed => {
                formatter.write_str("Anonymous invocation is not enabled for this Object")
            }
            Self::InvalidCallerId => {
                formatter.write_str("delegation resolver returned an invalid Caller ID")
            }
            Self::UnknownCallerKind => {
                formatter.write_str("delegation resolver returned an unknown Caller kind")
            }
            Self::Database(error) => write!(formatter, "Caller resolution database error: {error}"),
        }
    }
}

impl std::error::Error for CallerResolutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}
