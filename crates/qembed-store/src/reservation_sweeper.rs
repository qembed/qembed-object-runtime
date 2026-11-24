//! Worker-only PostgreSQL adapter for expiring safe-to-release token reservations.
//!
//! The database function is SECURITY DEFINER and executable only by qembed_worker.
//! This adapter is intentionally not tenant-scoped: one worker sweep spans tenants.
//! Production callers must use a dedicated qembed_worker login/pool, never the
//! ordinary qembed_runtime application pool.

use std::{
    error::Error,
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use sqlx::PgPool;

/// Errors returned by the reservation expiry sweeper.
#[derive(Debug)]
pub enum ReservationSweepError {
    InvalidBatchSize,
    InvalidCutoff,
    InvalidPersistedCount,
    Database(sqlx::Error),
}

impl fmt::Display for ReservationSweepError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBatchSize => {
                formatter.write_str("reservation sweep batch size must be between 1 and 500")
            }
            Self::InvalidCutoff => {
                formatter.write_str("reservation sweep cutoff must be a representable Unix timestamp")
            }
            Self::InvalidPersistedCount => {
                formatter.write_str("reservation sweep returned an invalid expired-row count")
            }
            Self::Database(error) => write!(formatter, "reservation sweep database error: {error}"),
        }
    }
}

impl Error for ReservationSweepError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// Executes the canonical cross-tenant reservation expiry function.
///
/// The supplied pool must authenticate as `qembed_worker`. The database
/// function locks provider-attempt rows with `SKIP LOCKED`, and releases only
/// expired reservations whose provider attempt proves that no request may have
/// been accepted. `MayHaveBeenSent` and `OutcomeUnknown` are never swept.
#[derive(Clone)]
pub struct PgReservationSweeper {
    pool: PgPool,
}

impl PgReservationSweeper {
    #[must_use]
    pub fn new(worker_pool: PgPool) -> Self {
        Self { pool: worker_pool }
    }

    /// Sweeps up to `batch_size` reservations using the current application clock.
    pub async fn sweep(
        &self,
        batch_size: u16,
    ) -> Result<u32, ReservationSweepError> {
        self.sweep_before(SystemTime::now(), batch_size).await
    }

    /// Deterministic-clock entry point for tests and controlled maintenance.
    ///
    /// The cutoff is passed to PostgreSQL; expiry decisions do not depend on
    /// when the statement happens to execute.
    pub async fn sweep_before(
        &self,
        cutoff: SystemTime,
        batch_size: u16,
    ) -> Result<u32, ReservationSweepError> {
        if !(1..=500).contains(&batch_size) {
            return Err(ReservationSweepError::InvalidBatchSize);
        }
        let cutoff_micros = system_time_to_micros(cutoff)?;
        let expired: i32 = sqlx::query_scalar(
            "SELECT sweep_expired_reservations(\u00241, to_timestamp(\u00242::double precision / 1000000.0))",
        )
        .bind(i32::from(batch_size))
        .bind(cutoff_micros)
        .fetch_one(&self.pool)
        .await
        .map_err(ReservationSweepError::Database)?;

        u32::try_from(expired).map_err(|_| ReservationSweepError::InvalidPersistedCount)
    }
}

fn system_time_to_micros(value: SystemTime) -> Result<i64, ReservationSweepError> {
    let micros = value
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ReservationSweepError::InvalidCutoff)?
        .as_micros();
    i64::try_from(micros).map_err(|_| ReservationSweepError::InvalidCutoff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn cutoff_conversion_preserves_microseconds() {
        let cutoff = UNIX_EPOCH + Duration::from_micros(1_767_398_400_123_456);
        assert_eq!(system_time_to_micros(cutoff), Ok(1_767_398_400_123_456));
    }

    #[test]
    fn cutoff_before_unix_epoch_is_rejected() {
        let cutoff = UNIX_EPOCH - Duration::from_secs(1);
        assert!(matches!(
            system_time_to_micros(cutoff),
            Err(ReservationSweepError::InvalidCutoff)
        ));
    }

    #[test]
    fn cutoff_outside_postgres_microsecond_range_is_rejected() {
        let cutoff = UNIX_EPOCH + Duration::from_secs(u64::MAX);
        assert!(matches!(
            system_time_to_micros(cutoff),
            Err(ReservationSweepError::InvalidCutoff)
        ));
    }
}
