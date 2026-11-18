//! Domain authentication flow that turns verified adapter identity into trusted scope.

use std::fmt;

use crate::ports::AuthenticationPort;
use crate::principal::{Principal, PrincipalError, PrincipalKind};
use crate::scope::{Scope, ScopeError};

/// Authenticates a tenant owner and establishes the trusted scope used by runtime.
///
/// The adapter is the credential-verification boundary. Its result is converted
/// into a non-deserializable `Principal` and `Scope` only after verification.
/// The composition root must wire a trusted adapter; request DTOs are never
/// accepted as an authenticated identity.
pub async fn authenticate_owner<P>(
    port: &P,
    credentials: P::Credentials,
) -> Result<Scope, OwnerAuthenticationError<P::Error>>
where
    P: AuthenticationPort,
{
    let identity = port
        .authenticate(credentials)
        .await
        .map_err(OwnerAuthenticationError::Rejected)?;

    if identity.principal_kind() != PrincipalKind::TenantOwner {
        return Err(OwnerAuthenticationError::NotOwner);
    }

    let principal = Principal::from_verified_auth(identity.principal_id(), identity.principal_kind())
        .map_err(OwnerAuthenticationError::InvalidPrincipal)?;

    Scope::from_verified_auth(identity.tenant_id(), principal)
        .map_err(OwnerAuthenticationError::InvalidScope)
}

/// Failure to authenticate or establish an owner scope.
#[derive(Debug)]
pub enum OwnerAuthenticationError<E> {
    /// Credentials were rejected or the authentication adapter failed.
    Rejected(E),
    /// The verified identity is not a tenant owner.
    NotOwner,
    /// The adapter returned an invalid caller identity.
    InvalidPrincipal(PrincipalError),
    /// The adapter returned an invalid tenant identity.
    InvalidScope(ScopeError),
}

impl<E: fmt::Display> fmt::Display for OwnerAuthenticationError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(error) => write!(formatter, "owner authentication rejected: {error}"),
            Self::NotOwner => formatter.write_str("authenticated principal is not a tenant owner"),
            Self::InvalidPrincipal(error) => {
                write!(formatter, "invalid authenticated principal: {error}")
            }
            Self::InvalidScope(error) => write!(formatter, "invalid authenticated scope: {error}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for OwnerAuthenticationError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected(error) => Some(error),
            Self::InvalidPrincipal(error) => Some(error),
            Self::InvalidScope(error) => Some(error),
            Self::NotOwner => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::future::ready;

    use crate::ids::{PrincipalId, TenantId};
    use crate::ports::{AuthenticatedIdentity, AuthenticationPort};

    use super::*;

    struct VerifiedOwner;

    impl AuthenticationPort for VerifiedOwner {
        type Credentials = ();
        type Error = Infallible;

        fn authenticate(
            &self,
            (): Self::Credentials,
        ) -> impl std::future::Future<Output = Result<AuthenticatedIdentity, Self::Error>> + Send
        {
            ready(Ok(AuthenticatedIdentity::new(
                TenantId::new(),
                PrincipalId::new(),
                PrincipalKind::TenantOwner,
            )))
        }
    }

    struct VerifiedMember;

    impl AuthenticationPort for VerifiedMember {
        type Credentials = ();
        type Error = Infallible;

        fn authenticate(
            &self,
            (): Self::Credentials,
        ) -> impl std::future::Future<Output = Result<AuthenticatedIdentity, Self::Error>> + Send
        {
            ready(Ok(AuthenticatedIdentity::new(
                TenantId::new(),
                CallerId::new(),
                PrincipalKind::Authenticated,
            )))
        }
    }

    #[tokio::test]
    async fn verified_owner_identity_becomes_trusted_scope() {
        let scope = authenticate_owner(&VerifiedOwner, ())
            .await
            .expect("verified owner should establish scope");

        assert_eq!(scope.principal().kind(), PrincipalKind::TenantOwner);
    }

    #[tokio::test]
    async fn verified_non_owner_identity_is_rejected() {
        let error = authenticate_owner(&VerifiedMember, ())
            .await
            .expect_err("ordinary authenticated principal is not an owner");

        assert!(matches!(error, OwnerAuthenticationError::NotOwner));
    }
}
