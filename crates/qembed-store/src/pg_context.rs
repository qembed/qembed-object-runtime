//! Application-owned PostgreSQL access with mandatory signed identity context.
//!
//! A `PgContext` is constructed once at the composition root from the pool and
//! the deployment's ContextIssuer. Store adapters receive this capability
//! instead of a bare PgPool, so production repository entry points can choose
//! only an explicit Principal-only or Caller-scoped signed transaction.
//!
//! The wrapped pool is private: external callers cannot obtain an unscoped
//! connection or silently fall back to legacy identity GUCs.

use qembed_domain::{ids::CallerId, scope::Scope};
use sqlx::PgPool;

use crate::{
    ContextIssuer, ContextTransaction, ContextTransactionError, begin_context_transaction,
    begin_principal_context_transaction,
};

/// PostgreSQL access capability that requires signed context for transactions.
#[derive(Clone)]
pub struct PgContext {
    pool: PgPool,
    issuer: ContextIssuer,
}

impl PgContext {
    /// Creates the trusted database capability at the application composition root.
    ///
    /// The issuer must be configured for this deployment and use a key whose
    /// public counterpart is registered in PostgreSQL's verifier key store.
    #[must_use]
    pub fn new(pool: PgPool, issuer: ContextIssuer) -> Self {
        Self { pool, issuer }
    }

    /// Begins a signed Principal-only transaction.
    ///
    /// No Caller is selected or inferred. Use this for tenant-scoped operations
    /// whose authorization does not require episodic-memory Caller visibility.
    pub async fn begin_principal(
        &self,
        scope: &Scope,
    ) -> Result<ContextTransaction, ContextTransactionError> {
        begin_principal_context_transaction(&self.pool, scope, &self.issuer).await
    }

    /// Begins a signed Caller-scoped transaction for an explicitly selected Caller.
    ///
    /// The Caller must be actively delegated to the authenticated Principal.
    /// This method never maps a Principal to a Caller implicitly.
    pub async fn begin_caller(
        &self,
        scope: &Scope,
        caller_id: CallerId,
    ) -> Result<ContextTransaction, ContextTransactionError> {
        begin_context_transaction(&self.pool, scope, caller_id, &self.issuer).await
    }
}

impl std::fmt::Debug for PgContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgContext")
            .field("pool", &"[PostgreSQL pool]")
            .field("issuer", &self.issuer)
            .finish()
    }
}
