//! Valkey adapter for the atomic distributed Token Bucket.

use std::{sync::Arc, time::Duration};

use redis::ErrorKind;

use qembed_domain::rate_limit::{RateLimitDecision, RateLimitKey, RateLimitPolicy, RateLimitStore};
use tokio::sync::OnceCell;

/// One Lua invocation performs time lookup, refill, decision, decrement, and TTL
/// refresh. No read/modify/write race exists between independent runtime workers.
const TOKEN_BUCKET_SCRIPT: &str = r#"
local time = redis.call('TIME')
local now_ms = tonumber(time[1]) * 1000 + math.floor(tonumber(time[2]) / 1000)
local capacity = tonumber(ARGV[1])
local refill_tokens = tonumber(ARGV[2])
local refill_period_ms = tonumber(ARGV[3])
local ttl_ms = tonumber(ARGV[4])
local cost = tonumber(ARGV[5])

local tokens = tonumber(redis.call('HGET', KEYS[1], 'tokens'))
local last_ms = tonumber(redis.call('HGET', KEYS[1], 'last_ms'))
if tokens == nil then tokens = capacity end
if last_ms == nil then last_ms = now_ms end

local effective_now = math.max(now_ms, last_ms)
local elapsed_ms = effective_now - last_ms
tokens = math.min(capacity, tokens + (elapsed_ms * refill_tokens / refill_period_ms))

local allowed = 0
local retry_ms = 0
if tokens >= cost then
  allowed = 1
  tokens = tokens - cost
else
  retry_ms = math.ceil((cost - tokens) * refill_period_ms / refill_tokens)
end

redis.call('HSET', KEYS[1], 'tokens', tostring(tokens), 'last_ms', tostring(effective_now))
redis.call('PEXPIRE', KEYS[1], ttl_ms)
return { allowed, math.floor(tokens), retry_ms }
"#;

/// Cloneable Valkey client with a lazily initialized, reconnecting connection manager.
#[derive(Clone)]
pub struct ValkeyRateLimitStore {
    client: redis::Client,
    connection: Arc<OnceCell<redis::aio::ConnectionManager>>,
}

impl ValkeyRateLimitStore {
    pub fn new(url: &str) -> Result<Self, redis::RedisError> {
        Ok(Self {
            client: redis::Client::open(url)?,
            connection: Arc::new(OnceCell::new()),
        })
    }

    async fn connection(&self) -> Result<&redis::aio::ConnectionManager, redis::RedisError> {
        self.connection
            .get_or_try_init(|| async {
                redis::aio::ConnectionManager::new(self.client.clone()).await
            })
            .await
    }
}

impl RateLimitStore for ValkeyRateLimitStore {
    type Error = redis::RedisError;

    async fn consume(
        &self,
        key: &RateLimitKey,
        policy: RateLimitPolicy,
        cost: u32,
    ) -> Result<RateLimitDecision, Self::Error> {
        let mut connection = self.connection().await?.clone();
        let (allowed, remaining, retry_after_ms): (i64, i64, i64) = redis::cmd("EVAL")
            .arg(TOKEN_BUCKET_SCRIPT)
            .arg(1)
            .arg(key.as_str())
            .arg(policy.capacity())
            .arg(policy.refill_tokens())
            .arg(policy.refill_period_ms())
            .arg(policy.idle_ttl_ms())
            .arg(cost)
            .query_async(&mut connection)
            .await?;

        if allowed != 0 && allowed != 1 {
            return Err(redis::RedisError::from((
                ErrorKind::TypeError,
                "Valkey returned an invalid rate-limit decision",
            )));
        }
        let remaining = u32::try_from(remaining).map_err(|_| {
            redis::RedisError::from((
                ErrorKind::TypeError,
                "Valkey returned invalid remaining tokens",
            ))
        })?;
        let retry_after_ms = u64::try_from(retry_after_ms).map_err(|_| {
            redis::RedisError::from((
                ErrorKind::TypeError,
                "Valkey returned invalid retry-after",
            ))
        })?;
        let retry_after = (allowed == 0).then(|| Duration::from_millis(retry_after_ms));
        Ok(RateLimitDecision::new(allowed == 1, remaining, retry_after))
    }
}
