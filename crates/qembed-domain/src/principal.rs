//! Authenticated principal identity used to establish tenant scope.

use std::fmt;

use crate::ids::PrincipalId;

/// The trust level established by the authentication boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrincipalKind {
    /// A caller authenticated without tenant-owner privileges.
    Authenticated,
    /// A caller authenticated as the tenant owner.
    TenantOwner,
    /// An unauthenticated caller represented by a server-issued caller ID.
    Anonymous,
}

/// Identity and trust level established by authentication.
///
/// Fields are private and there is deliberately no public constructor or
/// deserialization implementation. Only trusted code inside this crate can
/// mint a principal; request DTOs must never turn client-supplied fields into
/// an authenticated identity.
///
/// External callers cannot mint one:
///
/// ```compile_fail
/// use qembed_domain::ids::CallerId;
/// use qembed_domain::principal::{Principal, PrincipalKind};
///
/// let _ = Principal::from_verified_auth(PrincipalId::new(), PrincipalKind::Authenticated);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Principal {
    principal_id: PrincipalId,
    kind: PrincipalKind,
}

impl Principal {
    /// Creates a principal after the authentication boundary has verified it.
    ///
    /// This is crate-private so transport code cannot manufacture a principal
    /// directly from an HTTP request body.
    #[allow(dead_code)] // Used by the trusted authentication boundary as it is introduced.
    pub(crate) fn from_verified_auth(
        principal_id: PrincipalId,
        kind: PrincipalKind,
    ) -> Result<Self, PrincipalError> {
        if principal_id.as_uuid().is_nil() {
            return Err(PrincipalError::NilCallerId);
        }

        Ok(Self { principal_id, kind })
    }

    /// Returns the server-established caller identity.
    #[must_use]
    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    /// Returns the trust level established by authentication.
    #[must_use]
    pub const fn kind(&self) -> PrincipalKind {
        self.kind
    }
}

/// Errors produced while establishing a trusted principal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrincipalError {
    /// The authentication boundary supplied the nil UUID as caller identity.
    NilCallerId,
}

impl fmt::Display for PrincipalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NilCallerId => formatter.write_str("principal ID must not be nil"),
        }
    }
}

impl std::error::Error for PrincipalError {}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn verified_auth_context_creates_principal() {
        let caller_id = PrincipalId::new();

        let principal = Principal::from_verified_auth(caller_id, PrincipalKind::Authenticated)
            .expect("non-nil caller ID must be accepted");

        assert_eq!(principal.caller_id(), caller_id);
        assert_eq!(principal.kind(), PrincipalKind::Authenticated);
    }

    #[test]
    fn verified_auth_context_rejects_nil_caller_id() {
        let error = Principal::from_verified_auth(
            PrincipalId::from_uuid(Uuid::nil()),
            PrincipalKind::Authenticated,
        )
        .expect_err("nil caller ID must be rejected");

        assert_eq!(error, PrincipalError::NilCallerId);
    }
}
