//! PostgreSQL-backed tenant-owner API-token authentication.

use qembed_domain::ids::{PrincipalId, TenantId};
use qembed_domain::ports::{AuthenticatedIdentity, AuthenticationPort};
use qembed_domain::principal::PrincipalKind;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::fmt;

/// Credentials for the minimal Owner path.
///
/// The token is a high-entropy API credential, not a human password. Only its
/// SHA-256 digest is persisted. Debug output deliberately redacts the token.
pub struct OwnerCredentials {
    tenant_id: TenantId,
    token: String,
}

impl OwnerCredentials {
    /// Creates an authentication request for a tenant and its opaque owner token.
    #[must_use]
    pub fn new(tenant_id: TenantId, token: impl Into<String>) -> Self {
        Self {
            tenant_id,
            token: token.into(),
        }
    }
}

impl fmt::Debug for OwnerCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OwnerCredentials")
            .field("tenant_id", &self.tenant_id)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// SQLx adapter that verifies owner tokens against persisted principal records.
#[derive(Clone, Debug)]
pub struct OwnerAuthenticator {
    pool: PgPool,
}

impl OwnerAuthenticator {
    /// Creates the adapter using the application's PostgreSQL pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Authentication failures do not distinguish an unknown, disabled, revoked,
/// or tenant-mismatched credential. This avoids exposing account state.
#[derive(Debug)]
pub enum OwnerAuthenticationError {
    /// No active owner credential matched the supplied tenant and token.
    InvalidCredentials,
    /// PostgreSQL could not complete the lookup.
    Database(sqlx::Error),
}

impl fmt::Display for OwnerAuthenticationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCredentials => formatter.write_str("invalid credentials"),
            Self::Database(error) => {
                write!(formatter, "owner authentication database error: {error}")
            }
        }
    }
}

impl std::error::Error for OwnerAuthenticationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidCredentials => None,
            Self::Database(error) => Some(error),
        }
    }
}

impl AuthenticationPort for OwnerAuthenticator {
    type Credentials = OwnerCredentials;
    type Error = OwnerAuthenticationError;

    async fn authenticate(
        &self,
        credentials: Self::Credentials,
    ) -> Result<AuthenticatedIdentity, Self::Error> {
        let token_hash = Sha256::digest(credentials.token.as_bytes()).to_vec();

        // This lookup happens before a trusted Scope exists. The canonical
        // pre-auth owner tables intentionally have no tenant RLS policy; runtime
        // has only narrow column SELECT grants. The supplied tenant is constrained
        // in the same statement as the high-entropy token digest and is not treated
        // as authenticated identity until the persisted Principal is returned.
        let row = sqlx::query_as::<_, (String, String)>(
            r#"
            SELECT principal.tenant_id::text, principal.caller_id::text
            FROM owner_principals AS principal
            INNER JOIN owner_credentials AS credential
                ON credential.tenant_id = principal.tenant_id
                AND credential.caller_id = principal.caller_id
            WHERE principal.tenant_id = $1::uuid
                AND credential.token_hash = $2
                AND principal.principal_kind = 'tenant_owner'
                AND principal.disabled_at IS NULL
                AND credential.revoked_at IS NULL
            "#,
        )
        .bind(credentials.tenant_id.to_string())
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(OwnerAuthenticationError::Database)?;

        let Some((tenant_id, principal_id)) = row else {
            return Err(OwnerAuthenticationError::InvalidCredentials);
        };

        let tenant_id = tenant_id
            .parse::<TenantId>()
            .map_err(|_| OwnerAuthenticationError::InvalidCredentials)?;
        let principal_id = principal_id
            .parse::<PrincipalId>()
            .map_err(|_| OwnerAuthenticationError::InvalidCredentials)?;

        Ok(AuthenticatedIdentity::new(
            tenant_id,
            principal_id,
            PrincipalKind::TenantOwner,
        ))
    }
}

/// Credentials for any tenant Principal. The token is a high-entropy API
/// credential; only its SHA-256 digest is sent to PostgreSQL.
pub struct PrincipalCredentials {
    token: String,
}

impl PrincipalCredentials {
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self { token: token.into() }
    }
}

impl fmt::Debug for PrincipalCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrincipalCredentials")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// Authenticates against the canonical Principal registry.
#[derive(Clone, Debug)]
pub struct PrincipalAuthenticator {
    pool: PgPool,
}

impl PrincipalAuthenticator {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(Debug)]
pub enum PrincipalAuthenticationError {
    InvalidCredentials,
    Database(sqlx::Error),
}

impl fmt::Display for PrincipalAuthenticationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCredentials => formatter.write_str("invalid credentials"),
            Self::Database(error) => write!(formatter, "Principal authentication database error: {error}"),
        }
    }
}

impl std::error::Error for PrincipalAuthenticationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidCredentials => None,
            Self::Database(error) => Some(error),
        }
    }
}

impl AuthenticationPort for PrincipalAuthenticator {
    type Credentials = PrincipalCredentials;
    type Error = PrincipalAuthenticationError;

    async fn authenticate(
        &self,
        credentials: Self::Credentials,
    ) -> Result<AuthenticatedIdentity, Self::Error> {
        let token_hash = Sha256::digest(credentials.token.as_bytes()).to_vec();
        let row = sqlx::query_as::<_, (String, Option<String>, String)>(
            "SELECT principal_id::text, tenant_id::text, kind FROM authenticate_principal($1)",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(PrincipalAuthenticationError::Database)?;

        let Some((principal_id, tenant_id, kind)) = row else {
            return Err(PrincipalAuthenticationError::InvalidCredentials);
        };
        let Some(tenant_id) = tenant_id else {
            // PlatformAdmin is intentionally global and cannot establish a
            // tenant invocation Scope.
            return Err(PrincipalAuthenticationError::InvalidCredentials);
        };

        let principal_kind = match kind.as_str() {
            "TenantOwner" => PrincipalKind::TenantOwner,
            "TenantAdmin" | "Service" => PrincipalKind::Authenticated,
            "PlatformAdmin" => return Err(PrincipalAuthenticationError::InvalidCredentials),
            _ => return Err(PrincipalAuthenticationError::InvalidCredentials),
        };
        let tenant_id = tenant_id
            .parse::<TenantId>()
            .map_err(|_| PrincipalAuthenticationError::InvalidCredentials)?;
        let principal_id = principal_id
            .parse::<PrincipalId>()
            .map_err(|_| PrincipalAuthenticationError::InvalidCredentials)?;

        Ok(AuthenticatedIdentity::new(tenant_id, principal_id, principal_kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_credentials_debug_redacts_token() {
        let credentials = PrincipalCredentials::new("secret-principal-token");
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("secret-principal-token"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn owner_credentials_debug_redacts_token() {
        let credentials = OwnerCredentials::new(TenantId::new(), "secret-owner-token");

        let debug = format!("{credentials:?}");
        assert!(!debug.contains("secret-owner-token"));
        assert!(debug.contains("[REDACTED]"));
    }
}
