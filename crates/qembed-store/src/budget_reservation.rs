//! Atomic PostgreSQL token-budget reservation for begin-invocation.
//!
//! The object_budgets row serializes every reservation for an object. Daily
//! object usage is aggregate across pools; caller usage is caller-specific.
//! Pool identity is retained only for attribution. anonymous_share_pct is not
//! read by this module.

use std::fmt;

use qembed_domain::caller::InvocationPool;
use qembed_domain::ids::{CallerId, InvocationId, ObjectId};
use qembed_domain::scope::Scope;

use crate::{TenantTransaction, execute, fetch_optional};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetRejection {
    InvocationLimit,
    ObjectDailyLimit,
    CallerDailyLimit,
    ConcurrentInvocationLimit,
}

#[derive(Debug)]
pub enum BudgetReservationError {
    Rejected(BudgetRejection),
    InvalidEstimate,
    InvalidConfiguration,
    ArithmeticOverflow,
    Database(sqlx::Error),
}

impl fmt::Display for BudgetReservationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(reason) => write!(f, "token budget rejected invocation: {reason:?}"),
            Self::InvalidEstimate => f.write_str("token reservation estimate must be positive"),
            Self::InvalidConfiguration => f.write_str("object token budget configuration is invalid"),
            Self::ArithmeticOverflow => f.write_str("token budget arithmetic overflow"),
            Self::Database(error) => write!(f, "token budget reservation database error: {error}"),
        }
    }
}

impl std::error::Error for BudgetReservationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// Atomically checks and reserves the token estimate in the caller's begin
/// transaction. The reservation FK is deferred, so it may be inserted before
/// object_invocations; both rows and counter changes commit or roll back together.
pub(crate) async fn reserve_invocation_budget(
    transaction: &mut TenantTransaction,
    scope: &Scope,
    object_id: ObjectId,
    caller_id: CallerId,
    invocation_id: InvocationId,
    pool: InvocationPool,
    snapshot_limit: u64,
    estimate: u64,
) -> Result<(), BudgetReservationError> {
    if estimate == 0 {
        return Err(BudgetReservationError::InvalidEstimate);
    }

    // This lock is the single serialization point for object-wide daily and
    // concurrency ceilings. Never read counters before acquiring it.
    let budget = fetch_optional(
        transaction,
        sqlx::query_as::<_, (i64, i64, i32, i64, i32)>(
            "SELECT daily_token_limit, per_caller_daily_token_limit, max_concurrent_invocations, max_tokens_per_invocation, max_duration_ms FROM object_budgets WHERE tenant_id=$1::uuid AND object_id=$2::uuid FOR UPDATE",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string()),
    )
    .await
    .map_err(BudgetReservationError::Database)?
    .ok_or(BudgetReservationError::InvalidConfiguration)?;

    let (daily_limit, caller_limit, max_concurrent, live_invocation_limit, max_duration_ms) = budget;
    if daily_limit < 0
        || caller_limit < 0
        || max_concurrent <= 0
        || live_invocation_limit <= 0
        || max_duration_ms <= 0
    {
        return Err(BudgetReservationError::InvalidConfiguration);
    }

    let effective_limit = snapshot_limit.min(
        u64::try_from(live_invocation_limit)
            .map_err(|_| BudgetReservationError::InvalidConfiguration)?,
    );
    if effective_limit == 0 || estimate > effective_limit {
        return Err(BudgetReservationError::Rejected(BudgetRejection::InvocationLimit));
    }
    let estimate_i64 =
        i64::try_from(estimate).map_err(|_| BudgetReservationError::ArithmeticOverflow)?;

    let usage_date: String = sqlx::query_scalar(
        "SELECT (now() AT TIME ZONE 'UTC')::date::text",
    )
    .fetch_one(transaction.connection())
    .await
    .map_err(BudgetReservationError::Database)?;

    // Sum both pool partitions. The object budget lock prevents another
    // reservation path from changing either row until this transaction ends.
    let object_usage: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(consumed_tokens + reserved_tokens), 0)::bigint FROM usage_counters WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND usage_date=$3::date",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(usage_date)
    .fetch_one(transaction.connection())
    .await
    .map_err(BudgetReservationError::Database)?;
    let caller_usage: i64 = sqlx::query_scalar(
        "SELECT COALESCE(consumed_tokens + reserved_tokens, 0)::bigint FROM caller_usage_counters WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND caller_id=$3::uuid AND usage_date=$4::date",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(caller_id.to_string())
    .bind(usage_date)
    .fetch_optional(transaction.connection())
    .await
    .map_err(BudgetReservationError::Database)?
    .unwrap_or(0);

    let object_total = object_usage
        .checked_add(estimate_i64)
        .ok_or(BudgetReservationError::ArithmeticOverflow)?;
    if object_total > daily_limit {
        return Err(BudgetReservationError::Rejected(BudgetRejection::ObjectDailyLimit));
    }
    let caller_total = caller_usage
        .checked_add(estimate_i64)
        .ok_or(BudgetReservationError::ArithmeticOverflow)?;
    if caller_total > caller_limit {
        return Err(BudgetReservationError::Rejected(BudgetRejection::CallerDailyLimit));
    }

    let active_reservations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM budget_reservations WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND status='Reserved' AND expires_at > now()",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(BudgetReservationError::Database)?;
    if active_reservations >= i64::from(max_concurrent) {
        return Err(BudgetReservationError::Rejected(BudgetRejection::ConcurrentInvocationLimit));
    }

    // The same usage date is used for both accounting dimensions. Cost remains
    // explicitly outside this token-only stage; reserved_cost_micros is zero.
    execute(
        transaction,
        sqlx::query(
            "INSERT INTO usage_counters (tenant_id, object_id, usage_date, pool, reserved_tokens, reserved_cost_micros) VALUES ($1::uuid,$2::uuid,$3::date,$4,$5,0) ON CONFLICT (tenant_id,object_id,usage_date,pool) DO UPDATE SET reserved_tokens=usage_counters.reserved_tokens + EXCLUDED.reserved_tokens",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(usage_date)
        .bind(pool.as_sql())
        .bind(estimate_i64),
    )
    .await
    .map_err(BudgetReservationError::Database)?;

    execute(
        transaction,
        sqlx::query(
            "INSERT INTO caller_usage_counters (tenant_id,object_id,caller_id,usage_date,reserved_tokens) VALUES ($1::uuid,$2::uuid,$3::uuid,$4::date,$5) ON CONFLICT (tenant_id,object_id,caller_id,usage_date) DO UPDATE SET reserved_tokens=caller_usage_counters.reserved_tokens + EXCLUDED.reserved_tokens",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(caller_id.to_string())
        .bind(usage_date)
        .bind(estimate_i64),
    )
    .await
    .map_err(BudgetReservationError::Database)?;

    execute(
        transaction,
        sqlx::query(
            "INSERT INTO budget_reservations (id,tenant_id,object_id,invocation_id,pool,usage_date,reserved_tokens,reserved_cost_micros,status,expires_at) VALUES (gen_random_uuid(),$1::uuid,$2::uuid,$3::uuid,$4,$5::date,$6,0,'Reserved',now() + ($7::text || ' milliseconds')::interval)",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(invocation_id.to_string())
        .bind(pool.as_sql())
        .bind(usage_date)
        .bind(estimate_i64)
        .bind(max_duration_ms),
    )
    .await
    .map_err(BudgetReservationError::Database)?;

    Ok(())
}
