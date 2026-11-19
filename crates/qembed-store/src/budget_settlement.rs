//! Atomic settlement of a reserved invocation against normalized provider usage.

use std::fmt;

use qembed_domain::budget::settle_token_usage;
use qembed_domain::chat::TokenUsage;
use qembed_domain::ids::InvocationId;
use qembed_domain::scope::Scope;
use sqlx::PgPool;

use crate::{begin_tenant_transaction, execute, fetch_optional};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettlementOutcome {
    actual_tokens: u64,
    degraded: bool,
    replayed: bool,
}

impl SettlementOutcome {
    #[must_use]
    pub const fn actual_tokens(self) -> u64 {
        self.actual_tokens
    }

    #[must_use]
    pub const fn is_degraded(self) -> bool {
        self.degraded
    }

    /// True when the reservation had already been settled by an earlier attempt.
    #[must_use]
    pub const fn is_replay(self) -> bool {
        self.replayed
    }
}

#[derive(Debug)]
pub enum UsageSettlementError {
    NotFound,
    MissingBudgetConfiguration,
    MissingAccountingRow,
    InvalidReservationState,
    InvalidPersistedUsage,
    ArithmeticOverflow,
    Database(sqlx::Error),
}

impl fmt::Display for UsageSettlementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("invocation or budget reservation was not found"),
            Self::MissingBudgetConfiguration => {
                formatter.write_str("object budget configuration is missing")
            }
            Self::MissingAccountingRow => {
                formatter.write_str("token usage accounting row is missing or inconsistent")
            }
            Self::InvalidReservationState => {
                formatter.write_str("budget reservation is not eligible for settlement")
            }
            Self::InvalidPersistedUsage => {
                formatter.write_str("settled reservation has invalid persisted usage")
            }
            Self::ArithmeticOverflow => {
                formatter.write_str("provider token usage exceeds the supported accounting range")
            }
            Self::Database(error) => write!(
                formatter,
                "provider usage settlement database error: {error}"
            ),
        }
    }
}

impl std::error::Error for UsageSettlementError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// Replaces one invocation's estimate with normalized provider usage.
///
/// Lock order matches reservation creation: object budget, object/pool counter,
/// caller counter, reservation. The initial invocation lookup is read-only and
/// obtains the keys needed to follow that order. All accounting mutations and
/// the reservation state transition commit together.
pub async fn settle_invocation_usage(
    pool: &PgPool,
    scope: &Scope,
    invocation_id: InvocationId,
    provider_usage: Option<TokenUsage>,
) -> Result<SettlementOutcome, UsageSettlementError> {
    let mut transaction = begin_tenant_transaction(pool, scope)
        .await
        .map_err(UsageSettlementError::Database)?;

    let identity = fetch_optional(
        &mut transaction,
        sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT invocation.object_id::text, invocation.caller_id::text, invocation.pool, reservation.usage_date::text FROM object_invocations AS invocation JOIN budget_reservations AS reservation ON reservation.tenant_id=invocation.tenant_id AND reservation.object_id=invocation.object_id AND reservation.invocation_id=invocation.id WHERE invocation.tenant_id=$1::uuid AND invocation.id=$2::uuid",
        )
        .bind(scope.tenant_id().to_string())
        .bind(invocation_id.to_string()),
    )
    .await
    .map_err(UsageSettlementError::Database)?
    .ok_or(UsageSettlementError::NotFound)?;
    let (object_id, caller_id, invocation_pool, usage_date) = identity;

    let budget_exists: Option<(String,)> = fetch_optional(
        &mut transaction,
        sqlx::query_as(
            "SELECT object_id::text FROM object_budgets WHERE tenant_id=$1::uuid AND object_id=$2::uuid FOR UPDATE",
        )
        .bind(scope.tenant_id().to_string())
        .bind(&object_id),
    )
    .await
    .map_err(UsageSettlementError::Database)?;
    if budget_exists.is_none() {
        return Err(UsageSettlementError::MissingBudgetConfiguration);
    }

    let usage_counts: Option<(i64, i64)> = fetch_optional(
        &mut transaction,
        sqlx::query_as(
            "SELECT consumed_tokens, reserved_tokens FROM usage_counters WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND usage_date=$3::date AND pool=$4 FOR UPDATE",
        )
        .bind(scope.tenant_id().to_string())
        .bind(&object_id)
        .bind(&usage_date)
        .bind(&invocation_pool),
    )
    .await
    .map_err(UsageSettlementError::Database)?;
    let Some((_object_consumed, object_reserved)) = usage_counts else {
        return Err(UsageSettlementError::MissingAccountingRow);
    };

    let caller_counts: Option<(i64, i64)> = fetch_optional(
        &mut transaction,
        sqlx::query_as(
            "SELECT consumed_tokens, reserved_tokens FROM caller_usage_counters WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND caller_id=$3::uuid AND usage_date=$4::date FOR UPDATE",
        )
        .bind(scope.tenant_id().to_string())
        .bind(&object_id)
        .bind(&caller_id)
        .bind(&usage_date),
    )
    .await
    .map_err(UsageSettlementError::Database)?;
    let Some((_caller_consumed, caller_reserved)) = caller_counts else {
        return Err(UsageSettlementError::MissingAccountingRow);
    };

    let reservation: Option<(i64, Option<i64>, String)> = fetch_optional(
        &mut transaction,
        sqlx::query_as(
            "SELECT reserved_tokens, actual_tokens, status FROM budget_reservations WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND invocation_id=$3::uuid FOR UPDATE",
        )
        .bind(scope.tenant_id().to_string())
        .bind(&object_id)
        .bind(invocation_id.to_string()),
    )
    .await
    .map_err(UsageSettlementError::Database)?;
    let Some((reserved_tokens, actual_tokens, status)) = reservation else {
        return Err(UsageSettlementError::NotFound);
    };

    if status == "Settled" {
        let actual_tokens = actual_tokens.ok_or(UsageSettlementError::InvalidPersistedUsage)?;
        let degraded: bool = sqlx::query_scalar(
            "SELECT COALESCE(degraded_reason='provider_usage_missing', false) FROM object_invocations WHERE tenant_id=$1::uuid AND id=$2::uuid",
        )
        .bind(scope.tenant_id().to_string())
        .bind(invocation_id.to_string())
        .fetch_one(transaction.connection())
        .await
        .map_err(UsageSettlementError::Database)?;
        transaction
            .commit()
            .await
            .map_err(UsageSettlementError::Database)?;
        return Ok(SettlementOutcome {
            actual_tokens: u64::try_from(actual_tokens).map_err(|_| UsageSettlementError::InvalidPersistedUsage)?,
            degraded,
            replayed: true,
        });
    }
    if status != "Reserved" || reserved_tokens < 0 || actual_tokens.is_some() {
        return Err(UsageSettlementError::InvalidReservationState);
    }

    let reserved_u64 =
        u64::try_from(reserved_tokens).map_err(|_| UsageSettlementError::InvalidPersistedUsage)?;
    if object_reserved < reserved_tokens || caller_reserved < reserved_tokens {
        return Err(UsageSettlementError::MissingAccountingRow);
    }
    let settlement = settle_token_usage(reserved_u64, provider_usage);
    let actual_i64 = i64::try_from(settlement.actual_tokens())
        .map_err(|_| UsageSettlementError::ArithmeticOverflow)?;

    let object_update = execute(
        &mut transaction,
        sqlx::query(
            "UPDATE usage_counters SET reserved_tokens=reserved_tokens-$1, consumed_tokens=consumed_tokens+$2 WHERE tenant_id=$3::uuid AND object_id=$4::uuid AND usage_date=$5::date AND pool=$6 AND reserved_tokens >= $1 AND consumed_tokens <= 9223372036854775807-$2",
        )
        .bind(reserved_tokens)
        .bind(actual_i64)
        .bind(scope.tenant_id().to_string())
        .bind(&object_id)
        .bind(&usage_date)
        .bind(&invocation_pool),
    )
    .await
    .map_err(UsageSettlementError::Database)?;
    if object_update.rows_affected() != 1 {
        return Err(UsageSettlementError::MissingAccountingRow);
    }

    let caller_update = execute(
        &mut transaction,
        sqlx::query(
            "UPDATE caller_usage_counters SET reserved_tokens=reserved_tokens-$1, consumed_tokens=consumed_tokens+$2 WHERE tenant_id=$3::uuid AND object_id=$4::uuid AND caller_id=$5::uuid AND usage_date=$6::date AND reserved_tokens >= $1 AND consumed_tokens <= 9223372036854775807-$2",
        )
        .bind(reserved_tokens)
        .bind(actual_i64)
        .bind(scope.tenant_id().to_string())
        .bind(&object_id)
        .bind(&caller_id)
        .bind(&usage_date),
    )
    .await
    .map_err(UsageSettlementError::Database)?;
    if caller_update.rows_affected() != 1 {
        return Err(UsageSettlementError::MissingAccountingRow);
    }

    let reservation_update = execute(
        &mut transaction,
        sqlx::query(
            "UPDATE budget_reservations SET actual_tokens=$1, status='Settled' WHERE tenant_id=$2::uuid AND object_id=$3::uuid AND invocation_id=$4::uuid AND status='Reserved'",
        )
        .bind(actual_i64)
        .bind(scope.tenant_id().to_string())
        .bind(&object_id)
        .bind(invocation_id.to_string()),
    )
    .await
    .map_err(UsageSettlementError::Database)?;
    if reservation_update.rows_affected() != 1 {
        return Err(UsageSettlementError::InvalidReservationState);
    }

    if settlement.is_degraded() {
        execute(
            &mut transaction,
            sqlx::query(
                "UPDATE object_invocations SET degraded_reason='provider_usage_missing' WHERE tenant_id=$1::uuid AND id=$2::uuid AND degraded_reason IS NULL",
            )
            .bind(scope.tenant_id().to_string())
            .bind(invocation_id.to_string()),
        )
        .await
        .map_err(UsageSettlementError::Database)?;
    }

    transaction
        .commit()
        .await
        .map_err(UsageSettlementError::Database)?;
    Ok(SettlementOutcome {
        actual_tokens: settlement.actual_tokens(),
        degraded: settlement.is_degraded(),
        replayed: false,
    })
}
