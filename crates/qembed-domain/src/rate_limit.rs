//! Provider-neutral distributed request rate-limit contracts.

use std::{error::Error, future::Future, time::Duration};

use sha2::{Digest, Sha256};

use crate::{
    ids::{ObjectId, TenantId},
    principal::PrincipalKind,
    scope::Scope,
};

/// Independent request quota class. This is deliberately separate from
/// `InvocationPool`, which remains the PostgreSQL token-budget attribution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RateLimitClass {
    Anonymous,
    Authenticated,
    Owner,
}

impl RateLimitClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Anonymous => "anonymous",
            Self::Authenticated => "authenticated",
            Self::Owner => "owner",
        }
    }
}

/// A validated, opaque Valkey key. Tenant/object form a Redis Cluster hash tag
/// so related future keys can be placed in one slot.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RateLimitKey(String);

impl RateLimitKey {
    fn new(
        tenant_id: TenantId,
        object_id: ObjectId,
        class: RateLimitClass,
        subject_id: &str,
    ) -> Result<Self, RateLimitKeyError> {
        if subject_id.trim().is_empty() {
            return Err(RateLimitKeyError::EmptySubject);
        }
        let digest = Sha256::digest(subject_id.as_bytes());
        Ok(Self(format!(
            "qembed:rl:{{{}:{}}}:{}:{digest:x}",
            tenant_id,
            object_id,
            class.as_str()
        )))
    }

    /// Creates the Owner bucket only from an authenticated TenantOwner scope.
    pub fn for_owner(scope: &Scope, object_id: ObjectId) -> Result<Self, RateLimitKeyError> {
        if scope.principal().kind() != PrincipalKind::TenantOwner {
            return Err(RateLimitKeyError::OwnerScopeRequired);
        }
        Self::new(
            scope.tenant_id(),
            object_id,
            RateLimitClass::Owner,
            &scope.principal().principal_id().to_string(),
        )
    }

    /// Creates the Authenticated bucket from a trusted Principal scope.
    /// The selected Caller is intentionally absent: rotating delegated Callers
    /// must not let one Principal evade its request quota.
    pub fn for_authenticated(
        scope: &Scope,
        object_id: ObjectId,
    ) -> Result<Self, RateLimitKeyError> {
        if scope.principal().kind() != PrincipalKind::Authenticated {
            return Err(RateLimitKeyError::AuthenticatedScopeRequired);
        }
        Self::new(
            scope.tenant_id(),
            object_id,
            RateLimitClass::Authenticated,
            &scope.principal().principal_id().to_string(),
        )
    }

    /// Creates an Anonymous bucket from a server-derived subject fingerprint.
    /// Callers must derive this fingerprint at the trusted edge; raw network
    /// addresses are never placed in Valkey keys or logs.
    pub fn for_anonymous(
        tenant_id: TenantId,
        object_id: ObjectId,
        subject_fingerprint: [u8; 32],
    ) -> Result<Self, RateLimitKeyError> {
        let digest = Sha256::digest(subject_fingerprint);
        Ok(Self(format!(
            "qembed:rl:{{{}:{}}}:{}:{digest:x}",
            tenant_id,
            object_id,
            RateLimitClass::Anonymous.as_str()
        )))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Invalid rate-limit key provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateLimitKeyError {
    EmptySubject,
    OwnerScopeRequired,
    AuthenticatedScopeRequired,
}

impl std::fmt::Display for RateLimitKeyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySubject => formatter.write_str("rate-limit subject must not be empty"),
            Self::OwnerScopeRequired => {
                formatter.write_str("Owner rate-limit keys require a TenantOwner scope")
            }
            Self::AuthenticatedScopeRequired => {
                formatter.write_str("Authenticated rate-limit keys require an Authenticated Principal scope")
            }
        }
    }
}

impl Error for RateLimitKeyError {}

/// Token Bucket policy. The idle TTL must be long enough for an empty bucket
/// to refill completely; otherwise key expiry could incorrectly reset it full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimitPolicy {
    capacity: u32,
    refill_tokens: u32,
    refill_period_ms: u64,
    idle_ttl_ms: u64,
}

impl RateLimitPolicy {
    pub fn new(
        capacity: u32,
        refill_tokens: u32,
        refill_period: Duration,
        idle_ttl: Duration,
    ) -> Result<Self, RateLimitPolicyError> {
        if capacity == 0 || refill_tokens == 0 {
            return Err(RateLimitPolicyError::ZeroCapacityOrRefill);
        }
        let refill_period_ms = u64::try_from(refill_period.as_millis())
            .map_err(|_| RateLimitPolicyError::DurationOutOfRange)?;
        let idle_ttl_ms = u64::try_from(idle_ttl.as_millis())
            .map_err(|_| RateLimitPolicyError::DurationOutOfRange)?;
        if refill_period_ms == 0 || idle_ttl_ms == 0 {
            return Err(RateLimitPolicyError::ZeroDuration);
        }
        // Lua numbers are IEEE-754 doubles; reject durations that cannot be
        // represented exactly as integer milliseconds.
        const MAX_SAFE_INTEGER_MS: u64 = 9_007_199_254_740_991;
        if refill_period_ms > MAX_SAFE_INTEGER_MS || idle_ttl_ms > MAX_SAFE_INTEGER_MS {
            return Err(RateLimitPolicyError::DurationOutOfRange);
        }
        let full_refill_ms = (u128::from(capacity) * u128::from(refill_period_ms))
            .div_ceil(u128::from(refill_tokens));
        if u128::from(idle_ttl_ms) < full_refill_ms {
            return Err(RateLimitPolicyError::TtlShorterThanFullRefill);
        }
        Ok(Self {
            capacity,
            refill_tokens,
            refill_period_ms,
            idle_ttl_ms,
        })
    }

    #[must_use]
    pub const fn capacity(self) -> u32 {
        self.capacity
    }

    #[must_use]
    pub const fn refill_tokens(self) -> u32 {
        self.refill_tokens
    }

    #[must_use]
    pub const fn refill_period_ms(self) -> u64 {
        self.refill_period_ms
    }

    #[must_use]
    pub const fn idle_ttl_ms(self) -> u64 {
        self.idle_ttl_ms
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateLimitPolicyError {
    ZeroCapacityOrRefill,
    ZeroDuration,
    DurationOutOfRange,
    TtlShorterThanFullRefill,
}

impl std::fmt::Display for RateLimitPolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid rate-limit policy: {self:?}")
    }
}

impl Error for RateLimitPolicyError {}

/// Validated, independently configured policy for each request class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimitPolicies {
    anonymous: RateLimitPolicy,
    authenticated: RateLimitPolicy,
    owner: RateLimitPolicy,
}

impl RateLimitPolicies {
    #[must_use]
    pub const fn new(
        anonymous: RateLimitPolicy,
        authenticated: RateLimitPolicy,
        owner: RateLimitPolicy,
    ) -> Self {
        Self {
            anonymous,
            authenticated,
            owner,
        }
    }

    #[must_use]
    pub const fn for_class(self, class: RateLimitClass) -> RateLimitPolicy {
        match class {
            RateLimitClass::Anonymous => self.anonymous,
            RateLimitClass::Authenticated => self.authenticated,
            RateLimitClass::Owner => self.owner,
        }
    }
}

/// Result of one atomic Token Bucket consume operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimitDecision {
    allowed: bool,
    remaining: u32,
    retry_after: Option<Duration>,
}

impl RateLimitDecision {
    #[must_use]
    pub const fn new(allowed: bool, remaining: u32, retry_after: Option<Duration>) -> Self {
        Self {
            allowed,
            remaining,
            retry_after,
        }
    }

    #[must_use]
    pub const fn allowed(self) -> bool {
        self.allowed
    }

    #[must_use]
    pub const fn remaining(self) -> u32 {
        self.remaining
    }

    #[must_use]
    pub const fn retry_after(self) -> Option<Duration> {
        self.retry_after
    }
}

/// Atomic shared-state boundary. Implementations must refill, decide, consume,
/// and refresh TTL as one indivisible operation.
pub trait RateLimitStore: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn consume(
        &self,
        key: &RateLimitKey,
        policy: RateLimitPolicy,
        cost: u32,
    ) -> impl Future<Output = Result<RateLimitDecision, Self::Error>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ids::{PrincipalId, TenantId},
        principal::{Principal, PrincipalKind},
    };

    #[test]
    fn policy_rejects_zero_values_and_ttl_shorter_than_full_refill() {
        assert_eq!(
            RateLimitPolicy::new(0, 1, Duration::from_secs(1), Duration::from_secs(1)),
            Err(RateLimitPolicyError::ZeroCapacityOrRefill)
        );
        assert_eq!(
            RateLimitPolicy::new(10, 1, Duration::from_secs(1), Duration::from_secs(9)),
            Err(RateLimitPolicyError::TtlShorterThanFullRefill)
        );
    }

    #[test]
    fn classes_have_independent_keys_and_cluster_slot_scope() {
        let tenant = TenantId::new();
        let object = ObjectId::new();
        let anonymous = RateLimitKey::new(tenant, object, RateLimitClass::Anonymous, "a").unwrap();
        let authenticated =
            RateLimitKey::new(tenant, object, RateLimitClass::Authenticated, "a").unwrap();
        let owner = RateLimitKey::new(tenant, object, RateLimitClass::Owner, "a").unwrap();
        assert_ne!(anonymous, authenticated);
        assert_ne!(authenticated, owner);
        assert!(
            anonymous
                .as_str()
                .contains(&format!("{{{tenant}:{object}}}"))
        );
    }

    #[test]
    fn owner_and_authenticated_keys_require_matching_trusted_principal_kind() {
        let tenant = TenantId::new();
        let authenticated = Principal::from_verified_auth(
            PrincipalId::new(),
            PrincipalKind::Authenticated,
        )
        .unwrap();
        let authenticated_scope = Scope::from_verified_auth(tenant, authenticated).unwrap();
        let object = ObjectId::new();

        assert!(RateLimitKey::for_authenticated(&authenticated_scope, object).is_ok());
        assert_eq!(
            RateLimitKey::for_owner(&authenticated_scope, object),
            Err(RateLimitKeyError::OwnerScopeRequired)
        );

        let owner = Principal::from_verified_auth(
            PrincipalId::new(),
            PrincipalKind::TenantOwner,
        )
        .unwrap();
        let owner_scope = Scope::from_verified_auth(tenant, owner).unwrap();
        assert!(RateLimitKey::for_owner(&owner_scope, object).is_ok());
        assert_eq!(
            RateLimitKey::for_authenticated(&owner_scope, object),
            Err(RateLimitKeyError::AuthenticatedScopeRequired)
        );
    }

    #[test]
    fn anonymous_key_contains_only_server_fingerprint_digest() {
        let tenant = TenantId::new();
        let object = ObjectId::new();
        let fingerprint = [7_u8; 32];
        let key = RateLimitKey::for_anonymous(tenant, object, fingerprint).unwrap();
        assert!(key.as_str().starts_with(&format!(
            "qembed:rl:{{{tenant}:{object}}}:anonymous:"
        )));
        let other = RateLimitKey::for_anonymous(tenant, object, [8_u8; 32]).unwrap();
        assert_ne!(key, other);
    }
}
