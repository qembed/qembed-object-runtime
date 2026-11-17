//! Authenticated principal identity used to establish tenant scope.

use std::fmt;

use crate::ids::CallerId;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Principal {
    caller_id: CallerId,
    kind: PrincipalKind,
}

impl Principal {
    /// Creates a principal after the authentication boundary has verified it.
    ///
    /// This is crate-private so transport code cannot manufacture a principal
    /// directly from an HTTP request body.
    pub(crate) fn from_verified_auth(
        caller_id: CallerId,
        kind: PrincipalKind,
    ) -> Result<Self, PrincipalError> {
        if caller_id.as_uuid().is_nil() {
            return Err(PrincipalError::NilCallerId);
        }

        Ok(Self { caller_id, kind })
    }

    /// Returns the server-established caller identity.
    #[must_use]
    pub const fn caller_id(&self) -> CallerId {
        self.caller_id
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
            Self::NilCallerId => formatter.write_str("caller ID must not be nil"),
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
        let caller_id = CallerId::new();

        let principal =
            Principal::from_verified_auth(caller_id, PrincipalKind::Authenticated)
                .expect("non-nil caller ID must be accepted");

        assert_eq!(principal.caller_id(), caller_id);
        assert_eq!(principal.kind(), PrincipalKind::Authenticated);
    }

    #[test]
    fn verified_auth_context_rejects_nil_caller_id() {
        let error = Principal::from_verified_auth(
            CallerId::from_uuid(Uuid::nil()),
            PrincipalKind::Authenticated,
        )
        .expect_err("nil caller ID must be rejected");

        assert_eq!(error, PrincipalError::NilCallerId);
    }
}
