//! Tenant scope derived from a trusted principal.

use std::fmt;

use crate::ids::TenantId;
use crate::principal::Principal;

/// Tenant boundary attached to repository operations.
///
/// The tenant ID and principal cannot be supplied independently by transport
/// DTOs: the only constructor is crate-private and requires a trusted
/// `Principal` minted by the authentication boundary. Scope is intentionally
/// not deserializable.
///
/// External callers cannot construct a scope from request data:
///
/// ```compile_fail
/// use qembed_domain::ids::TenantId;
/// use qembed_domain::scope::Scope;
///
/// let _ = Scope::from_verified_auth(TenantId::new(), todo!());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Scope {
    tenant_id: TenantId,
    principal: Principal,
}

impl Scope {
    /// Builds a scope after authentication has established both tenant and
    /// principal identity.
    ///
    /// This is crate-private to prevent HTTP handlers from constructing a
    /// trusted scope directly from untrusted request data.
    #[allow(dead_code)] // Used by the trusted authentication boundary as it is introduced.
    pub(crate) fn from_verified_auth(
        tenant_id: TenantId,
        principal: Principal,
    ) -> Result<Self, ScopeError> {
        if tenant_id.as_uuid().is_nil() {
            return Err(ScopeError::NilTenantId);
        }

        Ok(Self {
            tenant_id,
            principal,
        })
    }

    /// Returns the authenticated tenant ID.
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Returns the principal established by authentication.
    #[must_use]
    pub const fn principal(&self) -> &Principal {
        &self.principal
    }
}

/// Errors produced while establishing a tenant scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeError {
    /// The authentication boundary supplied the nil UUID as tenant identity.
    NilTenantId,
}

impl fmt::Display for ScopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NilTenantId => formatter.write_str("tenant ID must not be nil"),
        }
    }
}

impl std::error::Error for ScopeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::CallerId;
    use crate::principal::{Principal, PrincipalKind};
    use uuid::Uuid;

    #[test]
    fn trusted_auth_context_creates_scope() {
        let tenant_id = TenantId::new();
        let caller_id = CallerId::new();
        let principal =
            Principal::from_verified_auth(caller_id, PrincipalKind::TenantOwner)
                .expect("non-nil caller ID must be accepted");

        let scope = Scope::from_verified_auth(tenant_id, principal)
            .expect("non-nil tenant ID must be accepted");

        assert_eq!(scope.tenant_id(), tenant_id);
        assert_eq!(scope.principal().caller_id(), caller_id);
        assert_eq!(scope.principal().kind(), PrincipalKind::TenantOwner);
    }

    #[test]
    fn scope_rejects_nil_tenant_id() {
        let principal = Principal::from_verified_auth(
            CallerId::new(),
            PrincipalKind::Authenticated,
        )
        .expect("non-nil caller ID must be accepted");

        let error = Scope::from_verified_auth(TenantId::from_uuid(Uuid::nil()), principal)
            .expect_err("nil tenant ID must be rejected");

        assert_eq!(error, ScopeError::NilTenantId);
    }
}
