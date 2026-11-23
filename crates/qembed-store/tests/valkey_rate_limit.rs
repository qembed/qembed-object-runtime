use std::{sync::Arc, time::Duration};

use qembed_domain::{
    ids::{ObjectId, TenantId},
    rate_limit::{RateLimitKey, RateLimitPolicy, RateLimitStore},
};
use qembed_store::valkey_rate_limit::ValkeyRateLimitStore;
use tokio::sync::Barrier;

async fn store() -> ValkeyRateLimitStore {
    let endpoint = std::env::var("VALKEY_URL")
        .unwrap_or_else(|_| "redis://127.0.0.1:56379/".to_owned());
    ValkeyRateLimitStore::connect(&endpoint)
        .await
        .expect("Valkey test service must be available")
}

fn key() -> RateLimitKey {
    RateLimitKey::for_anonymous(TenantId::new(), ObjectId::new(), [7_u8; 32]).unwrap()
}

fn policy(capacity: u32, refill_tokens: u32, refill_ms: u64, ttl_ms: u64) -> RateLimitPolicy {
    RateLimitPolicy::new(
        capacity,
        refill_tokens,
        Duration::from_millis(refill_ms),
        Duration::from_millis(ttl_ms),
    )
    .unwrap()
}

#[tokio::test]
async fn token_bucket_refills_and_returns_retry_after() {
    let store = store().await;
    let key = key();
    let policy = policy(2, 1, 2_000, 10_000);

    let first = store.consume(&key, policy, 2).await.unwrap();
    assert!(first.allowed());
    assert_eq!(first.remaining(), 0);

    let denied = store.consume(&key, policy, 1).await.unwrap();
    assert!(!denied.allowed());
    assert_eq!(denied.remaining(), 0);
    assert!(
        denied
            .retry_after()
            .is_some_and(|delay| delay > Duration::ZERO)
    );

    tokio::time::sleep(Duration::from_millis(2_100)).await;
    let refilled = store.consume(&key, policy, 1).await.unwrap();
    assert!(refilled.allowed());
    assert_eq!(refilled.remaining(), 0);
}

#[tokio::test]
async fn denied_request_does_not_consume_tokens_and_refreshes_idle_ttl() {
    let store = store().await;
    let key = key();
    let policy = policy(2, 1, 10_000, 30_000);

    assert!(store.consume(&key, policy, 1).await.unwrap().allowed());
    tokio::time::sleep(Duration::from_millis(150)).await;
    let denied = store.consume(&key, policy, 2).await.unwrap();
    assert!(!denied.allowed());
    assert_eq!(denied.remaining(), 1);

    let remaining_token = store.consume(&key, policy, 1).await.unwrap();
    assert!(remaining_token.allowed());
    assert_eq!(remaining_token.remaining(), 0);

    let client = redis::Client::open(
        std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://127.0.0.1:56379/".to_owned()),
    )
    .unwrap();
    let mut connection = client.get_multiplexed_async_connection().await.unwrap();
    let ttl_ms: i64 = redis::cmd("PTTL")
        .arg(key.as_str())
        .query_async(&mut connection)
        .await
        .unwrap();
    assert!(ttl_ms > 29_900 && ttl_ms <= 30_000);
}

#[tokio::test]
async fn independent_connections_never_over_admit_concurrent_requests() {
    const CONNECTIONS: usize = 8;
    const REQUESTS: usize = 64;
    const CAPACITY: u32 = 9;

    let key = key();
    let policy = policy(CAPACITY, 1, 60_000, 600_000);
    let mut stores = Vec::with_capacity(CONNECTIONS);
    for _ in 0..CONNECTIONS {
        stores.push(store().await);
    }
    let barrier = Arc::new(Barrier::new(REQUESTS));
    let mut requests = Vec::with_capacity(REQUESTS);

    for index in 0..REQUESTS {
        let store = stores[index % CONNECTIONS].clone();
        let key = key.clone();
        let barrier = Arc::clone(&barrier);
        requests.push(tokio::spawn(async move {
            barrier.wait().await;
            store.consume(&key, policy, 1).await.unwrap().allowed()
        }));
    }

    let mut admitted = 0_u32;
    for request in requests {
        admitted += u32::from(request.await.unwrap());
    }
    assert_eq!(admitted, CAPACITY);
}
