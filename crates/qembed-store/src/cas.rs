//! Compare-and-set execution for tenant-scoped repository updates.

use std::fmt;

use sqlx::postgres::{PgArguments, PgQueryResult, Postgres};
use sqlx::query::Query;

use crate::{TenantTransaction, execute};

/// Failure to perform a repository compare-and-set update.
#[derive(Debug)]
pub enum CasError {
    /// The expected row version/state/token no longer matches.
    Conflict,
    /// A CAS predicate changed more than one row, violating its uniqueness assumption.
    UnexpectedRowsAffected(u64),
    /// PostgreSQL rejected the statement or could not complete it.
    Database(sqlx::Error),
}

impl fmt::Display for CasError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => formatter.write_str("compare-and-set conflict"),
            Self::UnexpectedRowsAffected(count) => {
                write!(
                    formatter,
                    "compare-and-set changed {count} rows; expected exactly one"
                )
            }
            Self::Database(error) => write!(formatter, "compare-and-set database error: {error}"),
        }
    }
}

impl std::error::Error for CasError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Conflict | Self::UnexpectedRowsAffected(_) => None,
        }
    }
}

/// Executes a tenant-scoped conditional update and requires exactly one row.
///
/// A predicate matching no row is a stale-state conflict. More than one
/// affected row is treated as an invariant violation rather than success.
/// The statement always executes through the supplied tenant transaction.
pub async fn execute_cas(
    transaction: &mut TenantTransaction,
    query: Query<'_, Postgres, PgArguments>,
) -> Result<(), CasError> {
    let result: PgQueryResult = execute(transaction, query)
        .await
        .map_err(CasError::Database)?;

    match result.rows_affected() {
        1 => Ok(()),
        0 => Err(CasError::Conflict),
        count => Err(CasError::UnexpectedRowsAffected(count)),
    }
}
