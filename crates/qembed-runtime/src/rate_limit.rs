//! Runtime orchestration for distributed request rate limiting.

use std::error::Error;

use qembed_domain::rate_limit::{
    RateLimitDecision, RateLimitKey, RateLimitPolicy, RateLimitStore,
};

#[derive(Debug)]
pub enum RateLimitServiceError<E> {
    InvalidCost,
    Store(E),
}

impl<E: Error + 'static> std::fmt::Display for RateLimitServiceError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCost => formatter.write_str("rate-limit cost must be non-zero and fit the bucket"),
            Self::Store(error) => write!(formatter, "rate-limit store failed: {error}"),
        }
    }
}

impl<E: Error + 'static> Error for RateLimitServiceError<E> {}

#[derive(Clone, Debug)]
pub struct RateLimitService<S> {
    store: S,
}

impl<S: RateLimitStore> RateLimitService<S> {
    #[must_use]
    pub const fn new(store: S) -> Self {
        Self { store }
    }

    /// Consumes request cost from one independently isolated bucket.
    pub async fn check(
        &self,
        key: &RateLimitKey,
        policy: RateLimitPolicy,
        cost: u32,
    ) -> Result<RateLimitDecision, RateLimitServiceError<S::Error>> {
        if cost == 0 || cost > policy.capacity() {
            return Err(RateLimitServiceError::InvalidCost);
        }
        self.store
            .consume(key, policy, cost)
            .await
            .map_err(RateLimitServiceError::Store)
    }
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, future::ready, sync::{Arc, Mutex}, time::Duration};

    use qembed_domain::{
        ids::{ObjectId, TenantId},
        rate_limit::{RateLimitClass, RateLimitKey, RateLimitPolicy, RateLimitStore},
    };

    use super::*;

    #[derive(Clone, Default)]
    struct MemoryStore(Arc<Mutex<std::collections::HashMap<String, u32>>>);

    impl RateLimitStore for MemoryStore {
        type Error = Infallible;

        fn consume(
            &self,
            key: &RateLimitKey,
            policy: RateLimitPolicy,
            cost: u32,
        ) -> impl std::future::Future<Output = Result<RateLimitDecision, Self::Error>> + Send {
            let mut buckets = self.0.lock().unwrap();
            let remaining = buckets.entry(key.as_str().to_owned()).or_insert(policy.capacity());
            let allowed = *remaining >= cost;
            if allowed { *remaining -= cost; }
            ready(Ok(RateLimitDecision::new(
                allowed,
                *remaining,
                (!allowed).then_some(Duration::from_secs(1)),
            )))
        }
    }

    #[tokio::test]
    async fn anonymous_authenticated_and_owner_have_independent_capacity() {
        let service = RateLimitService::new(MemoryStore::default());
        let tenant = TenantId::new();
        let object = ObjectId::new();
        let policy = RateLimitPolicy::new(2, 1, Duration::from_secs(1), Duration::from_secs(2)).unwrap();
        for class in [RateLimitClass::Anonymous, RateLimitClass::Authenticated, RateLimitClass::Owner] {
            let key = RateLimitKey::new(tenant, object, class, "subject").unwrap();
            assert!(service.check(&key, policy, 2).await.unwrap().allowed());
            assert!(!service.check(&key, policy, 1).await.unwrap().allowed());
        }
    }

    #[tokio::test]
    async fn invalid_cost_is_rejected_before_touching_store() {
        let service = RateLimitService::new(MemoryStore::default());
        let key = RateLimitKey::new(TenantId::new(), ObjectId::new(), RateLimitClass::Owner, "owner").unwrap();
        let policy = RateLimitPolicy::new(1, 1, Duration::from_secs(1), Duration::from_secs(1)).unwrap();
        assert!(matches!(
            service.check(&key, policy, 0).await,
            Err(RateLimitServiceError::InvalidCost)
        ));
    }
}
