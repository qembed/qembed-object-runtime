use std::{sync::Arc, time::Duration};

use qembed_domain::{
    ids::{CallerId, ObjectId, TenantId},
    rate_limit::{RateLimitKey, RateLimitPolicy, RateLimitRequestId, RateLimitStore},
};
use qembed_store::valkey_rate_limit::ValkeyRateLimitStore;
use tokio::sync::Barrier;

async fn store() -> ValkeyRateLimitStore {
    let endpoint =
        std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://127.0.0.1:56379/".to_owned());
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
        if request.await.unwrap() {
            admitted += 1;
        }
    }
    assert_eq!(admitted, CAPACITY);
}

#[tokio::test]
async fn idempotent_replay_does_not_consume_a_second_token() {
    let store = store().await;
    let tenant = TenantId::new();
    let object = ObjectId::new();
    let caller = CallerId::new();
    let key = RateLimitKey::for_anonymous(tenant, object, [9_u8; 32]).unwrap();
    let request_id =
        RateLimitRequestId::for_invocation(tenant, object, caller, "same-request-key").unwrap();
    let policy = policy(1, 1, 60_000, 600_000);

    let first = store
        .consume_once(&key, policy, 1, &request_id, Duration::from_secs(86_400))
        .await
        .unwrap();
    assert!(first.allowed());
    assert!(!first.is_deduplicated());
    assert_eq!(first.remaining(), 0);

    // The marker is independent of bucket subject/class. A replay through
    // another bucket identity must not charge that bucket either.
    let other_bucket = RateLimitKey::for_anonymous(tenant, object, [10_u8; 32]).unwrap();
    let replay = store
        .consume_once(
            &other_bucket,
            policy,
            1,
            &request_id,
            Duration::from_secs(86_400),
        )
        .await
        .unwrap();
    assert!(replay.allowed());
    assert!(replay.is_deduplicated());
    assert_eq!(replay.remaining(), 1);

    let another_request =
        RateLimitRequestId::for_invocation(tenant, object, caller, "different-request-key")
            .unwrap();
    let fresh_on_other_bucket = store
        .consume_once(
            &other_bucket,
            policy,
            1,
            &another_request,
            Duration::from_secs(86_400),
        )
        .await
        .unwrap();
    assert!(fresh_on_other_bucket.allowed());
    assert!(!fresh_on_other_bucket.is_deduplicated());

    let third_request =
        RateLimitRequestId::for_invocation(tenant, object, caller, "third-request-key").unwrap();
    let denied = store
        .consume_once(&key, policy, 1, &third_request, Duration::from_secs(86_400))
        .await
        .unwrap();
    assert!(!denied.allowed());
    assert!(!denied.is_deduplicated());
}

#[tokio::test]
async fn concurrent_same_idempotency_key_is_charged_once_across_connections() {
    const CONNECTIONS: usize = 8;
    const REQUESTS: usize = 64;

    let tenant = TenantId::new();
    let object = ObjectId::new();
    let caller = CallerId::new();
    let key = RateLimitKey::for_anonymous(tenant, object, [11_u8; 32]).unwrap();
    let request_id =
        RateLimitRequestId::for_invocation(tenant, object, caller, "concurrent-idempotency-key")
            .unwrap();
    let policy = policy(1, 1, 60_000, 600_000);

    let mut stores = Vec::with_capacity(CONNECTIONS);
    for _ in 0..CONNECTIONS {
        stores.push(store().await);
    }
    let barrier = Arc::new(Barrier::new(REQUESTS));
    let mut requests = Vec::with_capacity(REQUESTS);

    for index in 0..REQUESTS {
        let store = stores[index % CONNECTIONS].clone();
        let key = key.clone();
        let request_id = request_id.clone();
        let barrier = Arc::clone(&barrier);
        requests.push(tokio::spawn(async move {
            barrier.wait().await;
            store
                .consume_once(&key, policy, 1, &request_id, Duration::from_secs(86_400))
                .await
                .unwrap()
        }));
    }

    let mut admitted = 0;
    let mut deduplicated = 0;
    for request in requests {
        let decision = request.await.unwrap();
        assert!(decision.allowed());
        if decision.is_deduplicated() {
            deduplicated += 1;
        } else {
            admitted += 1;
        }
    }
    assert_eq!(admitted, 1);
    assert_eq!(deduplicated, REQUESTS - 1);
}
