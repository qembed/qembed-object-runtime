//! PostgreSQL persistence infrastructure.

use std::time::Duration;

use qembed_domain::scope::Scope;
use sqlx::PgPool;
use sqlx::Transaction;
use sqlx::migrate::{MigrateError, Migrator};
use sqlx::postgres::{PgConnection, PgPoolOptions, Postgres};

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

/// A PostgreSQL transaction with its tenant context established transaction-locally.
///
/// The wrapped connection is available only while this value is borrowed. Commit
/// and rollback consume the wrapper so a tenant-scoped transaction cannot be
/// accidentally reused after it has ended.
pub struct TenantTransaction {
    transaction: Transaction<'static, Postgres>,
}

impl TenantTransaction {
    /// Returns the PostgreSQL connection carrying this transaction's tenant context.
    ///
    /// Queries must be executed through this connection, not through the pool,
    /// so they participate in the transaction and inherit its tenant setting.
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

/// Begins a transaction and binds it to the tenant from a trusted Scope.
///
/// PostgreSQL's transaction-local setting is applied with `set_config(..., true)`.
/// The value is passed as a bind parameter; it is never interpolated into SQL.
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
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(scope.tenant_id().to_string())
        .execute(&mut *transaction)
        .await?;

    Ok(TenantTransaction { transaction })
}
