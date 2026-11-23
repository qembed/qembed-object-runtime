//! Valkey-backed implementation of the domain rate-limit store contract.
//!
//! Each consume is one Lua operation: server time, refill, admission, token
//! consumption, state persistence, and idle TTL refresh are serialized by
//! Valkey. The adapter stores only ephemeral request-throttling state; durable
//! invocation and token-budget accounting remain in PostgreSQL.

use std::{error::Error, fmt, time::Duration};

use qembed_domain::rate_limit::{
    RateLimitDecision, RateLimitKey, RateLimitPolicy, RateLimitStore,
};
use redis::{aio::ConnectionManager, Script};

const TOKEN_BUCKET_SCRIPT: &str = r#"
local capacity = tonumber(ARGV[1])
local refill_tokens = tonumber(ARGV[2])
local refill_period_ms = tonumber(ARGV[3])
local idle_ttl_ms = tonumber(ARGV[4])
local cost = tonumber(ARGV[5])

local server_time = redis.call('TIME')
local now_ms = tonumber(server_time[1]) * 1000
    + math.floor(tonumber(server_time[2]) / 1000)

local tokens = tonumber(redis.call('HGET', KEYS[1], 'tokens'))
local updated_at_ms = tonumber(redis.call('HGET', KEYS[1], 'updated_at_ms'))
local remainder = tonumber(redis.call('HGET', KEYS[1], 'remainder'))

if not tokens then
    tokens = capacity
    updated_at_ms = now_ms
    remainder = 0
end

if now_ms > updated_at_ms and tokens < capacity then
    local elapsed_ms = now_ms - updated_at_ms
    local tokens_needed = capacity - tokens
    local ms_to_full = math.ceil(
        (tokens_needed * refill_period_ms - remainder) / refill_tokens
    )

    if elapsed_ms >= ms_to_full then
        tokens = capacity
        remainder = 0
    else
        local refill_numerator = elapsed_ms * refill_tokens + remainder
        local gained = math.floor(refill_numerator / refill_period_ms)
        remainder = refill_numerator - gained * refill_period_ms
        tokens = math.min(capacity, tokens + gained)
    end
    updated_at_ms = now_ms
elseif now_ms > updated_at_ms then
    updated_at_ms = now_ms
end

if tokens >= capacity then
    remainder = 0
end

local allowed = 0
local retry_after_ms = 0
if tokens >= cost then
    allowed = 1
    tokens = tokens - cost
else
    local deficit = cost - tokens
    retry_after_ms = math.ceil(
        (deficit * refill_period_ms - remainder) / refill_tokens
    )
end

redis.call(
    'HSET', KEYS[1],
    'tokens', tokens,
    'updated_at_ms', updated_at_ms,
    'remainder', remainder
)
redis.call('PEXPIRE', KEYS[1], idle_ttl_ms)

return { allowed, tokens, retry_after_ms }
"#;

/// Errors returned by the Valkey-backed rate-limit store.
#[derive(Debug)]
pub enum ValkeyRateLimitError {
    Redis(redis::RedisError),
    InvalidCost,
    InvalidPolicy,
    InvalidResponse(&'static str),
}

impl fmt::Display for ValkeyRateLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redis(error) => write!(formatter, "Valkey command failed: {error}"),
            Self::InvalidCost => {
                formatter.write_str("rate-limit cost must be non-zero and fit the bucket")
            }
            Self::InvalidPolicy => {
                formatter.write_str("rate-limit policy exceeds Valkey Lua exact-integer range")
            }
            Self::InvalidResponse(message) => {
                write!(formatter, "invalid Valkey rate-limit response: {message}")
            }
        }
    }
}

impl Error for ValkeyRateLimitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Redis(error) => Some(error),
            Self::InvalidCost | Self::InvalidPolicy | Self::InvalidResponse(_) => None,
        }
    }
}

impl From<redis::RedisError> for ValkeyRateLimitError {
    fn from(error: redis::RedisError) -> Self {
        Self::Redis(error)
    }
}

/// A cloneable Valkey adapter using redis-rs automatic reconnect management.
#[derive(Clone, Debug)]
pub struct ValkeyRateLimitStore {
    connection: ConnectionManager,
}

impl ValkeyRateLimitStore {
    /// Connects to one Redis/Valkey RESP endpoint and establishes the initial
    /// managed connection. Sentinel and Cluster routing are intentionally not
    /// enabled until the deployment topology is selected.
    pub async fn connect(endpoint: &str) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(endpoint)?;
        let connection = ConnectionManager::new(client).await?;
        Ok(Self { connection })
    }
}

impl RateLimitStore for ValkeyRateLimitStore {
    type Error = ValkeyRateLimitError;

    fn consume(
        &self,
        key: &RateLimitKey,
        policy: RateLimitPolicy,
        cost: u32,
    ) -> impl std::future::Future<Output = Result<RateLimitDecision, Self::Error>> + Send {
        let mut connection = self.connection.clone();
        async move {
            if cost == 0 || cost > policy.capacity() {
                return Err(ValkeyRateLimitError::InvalidCost);
            }

            // Lua numbers are IEEE-754 doubles. Bound every possible refill
            // product so integer arithmetic remains exact in the script.
            const MAX_SAFE_LUA_INTEGER: u128 = 9_007_199_254_740_991;
            if u128::from(policy.capacity()) * u128::from(policy.refill_period_ms())
                > MAX_SAFE_LUA_INTEGER
            {
                return Err(ValkeyRateLimitError::InvalidPolicy);
            }

            let (allowed, remaining, retry_after_ms): (i64, i64, i64) =
                Script::new(TOKEN_BUCKET_SCRIPT)
                    .key(key.as_str())
                    .arg(policy.capacity())
                    .arg(policy.refill_tokens())
                    .arg(policy.refill_period_ms())
                    .arg(policy.idle_ttl_ms())
                    .arg(cost)
                    .invoke_async(&mut connection)
                    .await?;

            if allowed != 0 && allowed != 1 {
                return Err(ValkeyRateLimitError::InvalidResponse(
                    "allowed flag must be 0 or 1",
                ));
            }
            let remaining = u32::try_from(remaining).map_err(|_| {
                ValkeyRateLimitError::InvalidResponse(
                    "remaining token count is negative or out of range",
                )
            })?;
            let retry_after = if retry_after_ms > 0 {
                Some(Duration::from_millis(u64::try_from(retry_after_ms).map_err(
                    |_| ValkeyRateLimitError::InvalidResponse("retry delay is out of range"),
                )?))
            } else if retry_after_ms == 0 {
                None
            } else {
                return Err(ValkeyRateLimitError::InvalidResponse(
                    "retry delay must not be negative",
                ));
            };

            Ok(RateLimitDecision::new(allowed == 1, remaining, retry_after))
        }
    }
}
