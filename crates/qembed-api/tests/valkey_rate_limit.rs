use std::{sync::Arc, time::Duration};

use qembed_api::adapters::valkey_rate_limit::ValkeyRateLimitStore;
use qembed_domain::{
    ids::{ObjectId, TenantId},
    rate_limit::{RateLimitClass, RateLimitKey, RateLimitPolicy, RateLimitStore},
};

fn setup() -> (ValkeyRateLimitStore, RateLimitPolicy) {
    let url = std::env::var("VALKEY_URL").expect("VALKEY_URL is set by scripts/test-postgres.sh");
    let store = ValkeyRateLimitStore::new(&url).expect("Valkey URL is valid");
    let policy = RateLimitPolicy::new(
        17,
        1,
        Duration::from_secs(3_600),
        Duration::from_secs(86_400),
    )
    .expect("test policy is valid");
    (store, policy)
}

#[tokio::test]
async fn independent_workers_share_one_atomic_bucket() {
    let (first, policy) = setup();
    let url = std::env::var("VALKEY_URL").unwrap();
    let stores = vec![
        first,
        ValkeyRateLimitStore::new(&url).unwrap(),
        ValkeyRateLimitStore::new(&url).unwrap(),
        ValkeyRateLimitStore::new(&url).unwrap(),
    ];
    let key = Arc::new(
        RateLimitKey::new(
            TenantId::new(),
            ObjectId::new(),
            RateLimitClass::Authenticated,
            "multi-worker-subject",
        )
        .unwrap(),
    );
    let mut tasks = Vec::new();
    for index in 0..80 {
        let store = stores[index % stores.len()].clone();
        let key = key.clone();
        tasks.push(tokio::spawn(async move {
            store
                .consume(&key, policy, 1)
                .await
                .expect("Valkey consume succeeds")
                .allowed()
        }));
    }
    let mut allowed = 0;
    for task in tasks {
        if task.await.expect("worker task completes") {
            allowed += 1;
        }
    }
    assert_eq!(allowed, 17, "all workers must consume from the same bucket");
}

#[tokio::test]
async fn anonymous_authenticated_and_owner_buckets_are_isolated() {
    let (store, _) = setup();
    let tenant = TenantId::new();
    let object = ObjectId::new();
    let policy =
        RateLimitPolicy::new(1, 1, Duration::from_secs(3_600), Duration::from_secs(3_600)).unwrap();

    let keys = [
        RateLimitKey::new(tenant, object, RateLimitClass::Anonymous, "same-subject").unwrap(),
        RateLimitKey::new(
            tenant,
            object,
            RateLimitClass::Authenticated,
            "same-subject",
        )
        .unwrap(),
        RateLimitKey::new(tenant, object, RateLimitClass::Owner, "same-subject").unwrap(),
    ];
    for key in &keys {
        assert!(store.consume(key, policy, 1).await.unwrap().allowed());
    }
    for key in &keys {
        assert!(!store.consume(key, policy, 1).await.unwrap().allowed());
    }
}

#[tokio::test]
async fn token_bucket_refills_and_reports_retry_after() {
    let url = std::env::var("VALKEY_URL").expect("VALKEY_URL is set by scripts/test-postgres.sh");
    let store = ValkeyRateLimitStore::new(&url).expect("Valkey URL is valid");
    let policy = RateLimitPolicy::new(1, 1, Duration::from_millis(100), Duration::from_secs(1))
        .expect("short refill policy is valid");
    let key = RateLimitKey::new(
        TenantId::new(),
        ObjectId::new(),
        RateLimitClass::Owner,
        "refill-subject",
    )
    .unwrap();

    assert!(store.consume(&key, policy, 1).await.unwrap().allowed());
    let denied = store.consume(&key, policy, 1).await.unwrap();
    assert!(!denied.allowed());
    let retry_after = denied
        .retry_after()
        .expect("denied decision has retry-after");
    assert!(retry_after <= Duration::from_millis(100));

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(store.consume(&key, policy, 1).await.unwrap().allowed());
}
