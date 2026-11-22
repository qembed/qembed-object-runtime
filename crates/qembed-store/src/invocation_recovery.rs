//! PostgreSQL-driven recovery for stale, queued invocation deliveries.
//!
//! The sweeper only marks the original Outbox row for replay. It does not
//! publish to NATS, alter the payload, or allocate a new delivery identity.

use std::{
    error::Error,
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use qembed_domain::{
    ids::{EventId, InvocationId},
    scope::Scope,
};
use crate::{PgContext, fetch_all};

/// One invocation whose original Outbox delivery was durably marked for replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplayedInvocationDelivery {
    invocation_id: InvocationId,
    delivery_id: EventId,
}

impl ReplayedInvocationDelivery {
    #[must_use]
    pub const fn invocation_id(self) -> InvocationId {
        self.invocation_id
    }

    #[must_use]
    pub const fn delivery_id(self) -> EventId {
        self.delivery_id
    }
}

/// Errors returned by the PostgreSQL invocation recovery sweeper.
#[derive(Debug)]
pub enum InvocationRecoveryError {
    InvalidProgressTimeout,
    InvalidBatchSize,
    InvalidCutoff,
    InvalidPersistedId,
    Context(crate::ContextTransactionError),
    Database(sqlx::Error),
}

impl fmt::Display for InvocationRecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProgressTimeout => {
                formatter.write_str("invocation progress timeout must be positive")
            }
            Self::InvalidBatchSize => {
                formatter.write_str("recovery batch size must be between 1 and 500")
            }
            Self::InvalidCutoff => {
                formatter.write_str("recovery cutoff must be a valid past timestamp")
            }
            Self::InvalidPersistedId => {
                formatter.write_str("recovery function returned an invalid persisted ID")
            }
            Self::Context(error) => {
                write!(
                    formatter,
                    "invocation recovery signed-context initialization failed: {error}"
                )
            }
            Self::Database(error) => {
                write!(
                    formatter,
                    "invocation recovery database operation failed: {error}"
                )
            }
        }
    }
}

impl Error for InvocationRecoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// Tenant-scoped PostgreSQL sweeper. The caller schedules it periodically.
#[derive(Clone)]
pub struct PgInvocationRecoverySweeper {
    context: PgContext,
}

impl PgInvocationRecoverySweeper {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }

    /// Sweeps invocations whose last progress is older than progress_timeout.
    pub async fn sweep(
        &self,
        scope: &Scope,
        progress_timeout: Duration,
        batch_size: u16,
    ) -> Result<Vec<ReplayedInvocationDelivery>, InvocationRecoveryError> {
        if progress_timeout.is_zero() {
            return Err(InvocationRecoveryError::InvalidProgressTimeout);
        }
        let stale_before = SystemTime::now()
            .checked_sub(progress_timeout)
            .ok_or(InvocationRecoveryError::InvalidCutoff)?;
        self.sweep_before(scope, stale_before, batch_size).await
    }

    /// Deterministic-clock entry point. The cutoff is evaluated against
    /// COALESCE(heartbeat_at, started_at) inside PostgreSQL.
    pub async fn sweep_before(
        &self,
        scope: &Scope,
        stale_before: SystemTime,
        batch_size: u16,
    ) -> Result<Vec<ReplayedInvocationDelivery>, InvocationRecoveryError> {
        if !(1..=500).contains(&batch_size) {
            return Err(InvocationRecoveryError::InvalidBatchSize);
        }
        let micros = stale_before
            .duration_since(UNIX_EPOCH)
            .map_err(|_| InvocationRecoveryError::InvalidCutoff)?
            .as_micros();
        let micros = i64::try_from(micros).map_err(|_| InvocationRecoveryError::InvalidCutoff)?;

        let mut transaction = self
            .context
            .begin_principal(scope)
            .await
            .map_err(InvocationRecoveryError::Context)?;
        let rows = fetch_all(
            &mut transaction,
            sqlx::query_as::<_, (String, String)>(
                "SELECT replayed_invocation_id::text, replayed_delivery_id::text                  FROM sweep_stale_invocation_deliveries(                    to_timestamp($1::double precision / 1000000.0), $2                 )",
            )
            .bind(micros)
            .bind(i32::from(batch_size)),
        )
        .await
        .map_err(InvocationRecoveryError::Database)?;

        let replayed = rows
            .into_iter()
            .map(|(invocation_id, delivery_id)| {
                Ok(ReplayedInvocationDelivery {
                    invocation_id: invocation_id
                        .parse()
                        .map_err(|_| InvocationRecoveryError::InvalidPersistedId)?,
                    delivery_id: delivery_id
                        .parse()
                        .map_err(|_| InvocationRecoveryError::InvalidPersistedId)?,
                })
            })
            .collect::<Result<Vec<_>, InvocationRecoveryError>>()?;

        transaction
            .commit()
            .await
            .map_err(InvocationRecoveryError::Database)?;
        Ok(replayed)
    }
}
