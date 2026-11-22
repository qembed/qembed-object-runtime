//! PostgreSQL persistence infrastructure.
//!
//! Event append operations are deliberately internal to the store adapter. The
//! public API does not expose a generic event writer or mutable event repository:
//!
//! ```compile_fail
//! use qembed_store::event_outbox::append_object_event;
//! ```

pub mod auth;
pub mod begin_invocation;
mod budget_reservation;
pub mod budget_settlement;
pub mod caller;
mod cas;
pub mod context_issuer;
mod event_outbox;
pub mod idempotency;
pub mod invocation_cancellation;
mod invocation_lease;
pub mod invocation_query;
pub mod invocation_recovery;
pub mod invocation_start;
pub mod invocation_worker;
pub mod object_repository;
pub mod outbox_dispatcher;
pub mod readiness;
pub mod snapshot_repository;
pub mod tool_execution_store;
pub use cas::{CasError, execute_cas};
pub use context_issuer::{
    ContextIssuer, ContextIssuerConfig, ContextIssuerError, ContextTransaction,
    ContextTransactionError, RingEd25519SigningKey, begin_context_transaction,
    begin_principal_context_transaction,
};
pub use readiness::{PgReadinessFailure, PgReadinessProbe};

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use qembed_domain::ids::{PrincipalId, TenantId};
use qembed_domain::scope::Scope;
use sqlx::FromRow;
use sqlx::PgPool;
use sqlx::Transaction;
use sqlx::migrate::{MigrateError, Migrator};
use sqlx::postgres::{PgArguments, PgConnection, PgPoolOptions, PgQueryResult, PgRow, Postgres};
use sqlx::query::{Query, QueryAs};

/// Connection-pool settings shared by application and migration entry points.
#[derive(Clone, Debug)]
pub struct PgPoolConfig {
    /// Maximum number of simultaneously open PostgreSQL connections.
    pub max_connections: u32,
    /// Maximum time to wait when acquiring a connection from the pool.
    pub acquire_timeout: Duration,
}

impl Default for PgPoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 10,
            acquire_timeout: Duration::from_secs(5),
        }
    }
}

/// Create a PostgreSQL pool without applying or modifying any database schema.
pub async fn connect_pool(database_url: &str, config: PgPoolConfig) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(config.acquire_timeout)
        .connect(database_url)
        .await
}

/// Apply the migrations bundled with this crate.
///
/// This function is intentionally explicit: connecting to PostgreSQL never runs
/// migrations as a side effect.
pub async fn run_migrations(pool: &PgPool) -> Result<(), MigrateError> {
    let migrations_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let migrator = Migrator::new(migrations_path).await?;
    migrator.run(pool).await
}

/// A PostgreSQL transaction with trusted tenant and authenticated Principal context established transaction-locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TransactionToken(u64);

static NEXT_TRANSACTION_TOKEN: AtomicU64 = AtomicU64::new(1);

pub struct TenantTransaction {
    transaction: Transaction<'static, Postgres>,
    tenant_id: TenantId,
    principal_id: PrincipalId,
    transaction_token: TransactionToken,
}

impl TenantTransaction {
    pub(crate) const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    pub(crate) const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    pub(crate) const fn transaction_token(&self) -> TransactionToken {
        self.transaction_token
    }

    pub(crate) fn matches_scope(&self, scope: &Scope) -> bool {
        self.tenant_id == scope.tenant_id() && self.principal_id == scope.principal().principal_id()
    }

    /// Returns the PostgreSQL connection carrying this transaction's tenant context.
    pub fn connection(&mut self) -> &mut PgConnection {
        &mut self.transaction
    }

    /// Commits the tenant-scoped transaction.
    pub async fn commit(self) -> Result<(), sqlx::Error> {
        self.transaction.commit().await
    }

    /// Rolls back the tenant-scoped transaction.
    pub async fn rollback(self) -> Result<(), sqlx::Error> {
        self.transaction.rollback().await
    }
}

/// Begins a transaction and binds tenant and authenticated Principal from a trusted Scope.
///
/// PostgreSQL's transaction-local setting is applied with `set_config(..., true)`.
/// Both values are passed as bind parameters; neither is interpolated into SQL.
///
/// A scope is mandatory. Callers cannot begin a tenant transaction without one:
///
/// ```compile_fail
/// use qembed_store::begin_tenant_transaction;
///
/// async fn missing_scope(pool: &sqlx::PgPool) {
///     let _ = begin_tenant_transaction(pool, None).await;
/// }
/// ```
pub async fn begin_tenant_transaction(
    pool: &PgPool,
    scope: &Scope,
) -> Result<TenantTransaction, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "SELECT set_config('app.tenant_id', $1, true), set_config('app.principal_id', $2, true)",
    )
    .bind(scope.tenant_id().to_string())
    .bind(scope.principal().principal_id().to_string())
    .execute(&mut *transaction)
    .await?;

    Ok(TenantTransaction {
        transaction,
        tenant_id: scope.tenant_id(),
        principal_id: scope.principal().principal_id(),
        transaction_token: TransactionToken(NEXT_TRANSACTION_TOKEN.fetch_add(1, Ordering::Relaxed)),
    })
}

/// Begins a signed-context transaction with READ COMMITTED isolation.
///
/// Key revocation must become visible to the next statement that verifies the
/// token. A transaction-level repeatable-read snapshot could otherwise continue
/// observing a pre-revocation key row until the transaction ends.
pub(crate) async fn begin_context_tenant_transaction(
    pool: &PgPool,
    scope: &Scope,
) -> Result<TenantTransaction, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "SELECT set_config('app.tenant_id', $1, true), set_config('app.principal_id', $2, true)",
    )
    .bind(scope.tenant_id().to_string())
    .bind(scope.principal().principal_id().to_string())
    .execute(&mut *transaction)
    .await?;

    Ok(TenantTransaction {
        transaction,
        tenant_id: scope.tenant_id(),
        principal_id: scope.principal().principal_id(),
        transaction_token: TransactionToken(NEXT_TRANSACTION_TOKEN.fetch_add(1, Ordering::Relaxed)),
    })
}

/// Executes a typed row query inside a tenant-scoped transaction.
///
/// Repository methods should accept `&Scope`, begin their work with
/// [`begin_tenant_transaction`], and execute every tenant-owned query through
/// this helper (or its optional-row counterpart). The transaction is the
/// capability that carries the trusted tenant context; a bare pool is not
/// accepted.
///
/// The query retains SQLx's typed row mapping and bind-parameter API.
///
/// ```compile_fail
/// use qembed_store::fetch_all;
/// use sqlx::PgPool;
///
/// async fn unscoped(pool: &PgPool) {
///     let _ = fetch_all(pool, sqlx::query_as::<_, (String,)>("SELECT 'x'"));
/// }
/// ```
pub async fn fetch_all<'q, O>(
    transaction: &mut TenantTransaction,
    query: QueryAs<'q, Postgres, O, PgArguments>,
) -> Result<Vec<O>, sqlx::Error>
where
    O: for<'r> FromRow<'r, PgRow> + Send + Unpin,
{
    query.fetch_all(transaction.connection()).await
}

/// Fetches at most one typed row inside a tenant-scoped transaction.
pub async fn fetch_optional<'q, O>(
    transaction: &mut TenantTransaction,
    query: QueryAs<'q, Postgres, O, PgArguments>,
) -> Result<Option<O>, sqlx::Error>
where
    O: for<'r> FromRow<'r, PgRow> + Send + Unpin,
{
    query.fetch_optional(transaction.connection()).await
}

/// Executes a SQLx query inside a tenant-scoped transaction.
pub async fn execute<'q>(
    transaction: &mut TenantTransaction,
    query: Query<'q, Postgres, PgArguments>,
) -> Result<PgQueryResult, sqlx::Error> {
    query.execute(transaction.connection()).await
}
