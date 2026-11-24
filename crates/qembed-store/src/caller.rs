//! PostgreSQL-backed resolution of an explicitly selected invocation Caller.

use std::fmt;

use qembed_domain::anonymous::AnonymousContext;
use qembed_domain::caller::CallerKind;
use qembed_domain::ids::{CallerId, ObjectId, PrincipalId, TenantId};

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
    principal_id: PrincipalId,
    transaction_token: TransactionToken,
}

impl ResolvedCaller {
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
            && Some(self.principal_id) == transaction.principal_id()
            && self.transaction_token == transaction.transaction_token()
            && transaction.matches_scope(scope)
    }
}

/// A database-resolved Anonymous Caller bound to one signed Object transaction.
///
/// The stable Caller ID belongs to the public Object. The per-request
/// fingerprint remains separate and is used for rate-limit and idempotency
/// namespaces; it is never treated as a Principal identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedAnonymousCaller {
    caller_id: CallerId,
    kind: CallerKind,
    tenant_id: TenantId,
    object_id: ObjectId,
    subject_fingerprint: [u8; 32],
    transaction_token: TransactionToken,
}

impl ResolvedAnonymousCaller {
    #[must_use]
    pub const fn caller_id(self) -> CallerId {
        self.caller_id
    }

    #[must_use]
    pub const fn kind(self) -> CallerKind {
        self.kind
    }

    pub(crate) fn is_bound_to(
        &self,
        transaction: &TenantTransaction,
        context: &AnonymousContext,
    ) -> bool {
        self.tenant_id == context.tenant_id()
            && self.object_id == context.object_id()
            && self.subject_fingerprint == context.subject_fingerprint()
            && self.transaction_token == transaction.transaction_token()
            && transaction.matches_anonymous_context(context)
    }
}

/// Resolves the Object-scoped Anonymous Caller under a signed qctx2 invocation
/// context. The SQL function locks Object and Caller lifecycle rows until the
/// enclosing transaction commits.
pub async fn resolve_anonymous_caller(
    transaction: &mut TenantTransaction,
    context: &AnonymousContext,
    caller_id: CallerId,
) -> Result<ResolvedAnonymousCaller, AnonymousCallerResolutionError> {
    if !transaction.matches_anonymous_context(context) {
        return Err(AnonymousCallerResolutionError::ContextMismatch);
    }
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT caller_id::text, caller_kind FROM qembed_crypto.resolve_anonymous_caller($1::uuid, $2::uuid, $3::uuid)",
    )
    .bind(context.tenant_id().to_string())
    .bind(context.object_id().to_string())
    .bind(caller_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| match &error {
        sqlx::Error::Database(database_error)
            if database_error.code().as_deref() == Some("42501") =>
        {
            AnonymousCallerResolutionError::NotPubliclyInvokable
        }
        _ => AnonymousCallerResolutionError::Database(error),
    })?
    .ok_or(AnonymousCallerResolutionError::NotPubliclyInvokable)?;

    let resolved_id = row
        .0
        .parse::<CallerId>()
        .map_err(|_| AnonymousCallerResolutionError::InvalidCallerId)?;
    if resolved_id != caller_id {
        return Err(AnonymousCallerResolutionError::ContextMismatch);
    }
    let kind =
        CallerKind::parse(&row.1).ok_or(AnonymousCallerResolutionError::UnknownCallerKind)?;
    if kind != CallerKind::Anonymous {
        return Err(AnonymousCallerResolutionError::UnexpectedCallerKind);
    }

    Ok(ResolvedAnonymousCaller {
        caller_id: resolved_id,
        kind,
        tenant_id: context.tenant_id(),
        object_id: context.object_id(),
        subject_fingerprint: context.subject_fingerprint(),
        transaction_token: transaction.transaction_token(),
    })
}

/// Failure to resolve an Object-scoped Anonymous Caller.
#[derive(Debug)]
pub enum AnonymousCallerResolutionError {
    ContextMismatch,
    NotPubliclyInvokable,
    InvalidCallerId,
    UnknownCallerKind,
    UnexpectedCallerKind,
    Database(sqlx::Error),
}

impl fmt::Display for AnonymousCallerResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ContextMismatch => formatter.write_str("Anonymous Caller context mismatch"),
            Self::NotPubliclyInvokable => formatter.write_str("Object is not publicly invokable"),
            Self::InvalidCallerId => {
                formatter.write_str("Anonymous Caller resolver returned an invalid ID")
            }
            Self::UnknownCallerKind => {
                formatter.write_str("Anonymous Caller resolver returned an unknown kind")
            }
            Self::UnexpectedCallerKind => formatter.write_str("resolved Caller is not Anonymous"),
            Self::Database(error) => write!(
                formatter,
                "Anonymous Caller resolution database error: {error}"
            ),
        }
    }
}

impl std::error::Error for AnonymousCallerResolutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
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
        principal_id,
        transaction_token: transaction.transaction_token(),
    })
}

/// Failure to resolve an effective Caller.
#[derive(Debug)]
pub enum CallerResolutionError {
    /// The requested Caller is not actively delegated to this Principal.
    NotDelegated,
    /// Delegation resolution was attempted from an Anonymous transaction.
    PrincipalContextRequired,
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
                formatter.write_str("delegated Caller resolution requires a Principal transaction")
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
