//! Cryptographically signed PostgreSQL transaction context.
//!
//! The issuer signs canonical JSON with Ed25519. PostgreSQL receives only the
//! public verification material's key identifier and the signed token envelope;
//! private signing material never enters SQL parameters or GUCs.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use qembed_domain::ids::CallerId;
use qembed_domain::scope::Scope;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::caller::{CallerResolutionError, ResolvedCaller, resolve_delegated_caller};
use crate::{TenantTransaction, begin_tenant_transaction};

const TOKEN_PREFIX: &str = "qctx1";
const MAX_PAYLOAD_BYTES: usize = 8 * 1024;
const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(30);
const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(60);

/// Deployment and protocol identity trusted by the server composition root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextIssuerConfig {
    issuer: String,
    audience: String,
    deployment_id: String,
    key_id: String,
    token_lifetime: Duration,
}

impl ContextIssuerConfig {
    /// Creates a version-1 issuer configuration with a lifetime of 1..=60 seconds.
    pub fn new(
        issuer: impl Into<String>,
        audience: impl Into<String>,
        deployment_id: impl Into<String>,
        key_id: impl Into<String>,
        token_lifetime: Duration,
    ) -> Result<Self, ContextIssuerError> {
        let issuer = issuer.into();
        let audience = audience.into();
        let deployment_id = deployment_id.into();
        let key_id = key_id.into();

        if issuer.is_empty()
            || audience.is_empty()
            || deployment_id.is_empty()
            || key_id.is_empty()
            || token_lifetime.is_zero()
            || token_lifetime > MAX_TOKEN_LIFETIME
        {
            return Err(ContextIssuerError::InvalidConfiguration);
        }

        Ok(Self {
            issuer,
            audience,
            deployment_id,
            key_id,
            token_lifetime,
        })
    }

    /// Creates the recommended 30-second configuration.
    pub fn with_default_lifetime(
        issuer: impl Into<String>,
        audience: impl Into<String>,
        deployment_id: impl Into<String>,
        key_id: impl Into<String>,
    ) -> Result<Self, ContextIssuerError> {
        Self::new(issuer, audience, deployment_id, key_id, DEFAULT_TOKEN_LIFETIME)
    }
}

/// Ed25519 detached-signing capability. Implementations sign exact input bytes.
pub trait ContextSigningKey: Send + Sync {
    /// Returns the configured public-key identifier.
    fn key_id(&self) -> &str;

    /// Signs exact payload bytes and returns a 64-byte Ed25519 signature.
    fn sign(&self, message: &[u8]) -> Result<[u8; 64], ContextIssuerError>;
}

/// In-process Ed25519 signer backed by ring.
///
/// The caller must load the 32-byte seed from an approved secret source. This
/// type does not define KMS/HSM custody, rotation, or revocation policy.
pub struct RingEd25519SigningKey {
    key_id: String,
    key_pair: Ed25519KeyPair,
}

impl RingEd25519SigningKey {
    /// Builds a signer from an Ed25519 32-byte private seed and public key ID.
    pub fn from_seed(
        key_id: impl Into<String>,
        seed: &[u8; 32],
    ) -> Result<Self, ContextIssuerError> {
        let key_id = key_id.into();
        if key_id.is_empty() {
            return Err(ContextIssuerError::InvalidConfiguration);
        }
        let key_pair = Ed25519KeyPair::from_seed_unchecked(seed)
            .map_err(|_| ContextIssuerError::InvalidSigningKey)?;
        Ok(Self { key_id, key_pair })
    }

    /// Returns the corresponding 32-byte Ed25519 public key.
    #[must_use]
    pub fn public_key(&self) -> [u8; 32] {
        let bytes = self.key_pair.public_key().as_ref();
        let mut public_key = [0_u8; 32];
        public_key.copy_from_slice(bytes);
        public_key
    }
}

impl ContextSigningKey for RingEd25519SigningKey {
    fn key_id(&self) -> &str {
        &self.key_id
    }

    fn sign(&self, message: &[u8]) -> Result<[u8; 64], ContextIssuerError> {
        let signature = self.key_pair.sign(message);
        let mut bytes = [0_u8; 64];
        bytes.copy_from_slice(signature.as_ref());
        Ok(bytes)
    }
}

impl fmt::Debug for RingEd25519SigningKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RingEd25519SigningKey")
            .field("key_id", &self.key_id)
            .field("private_key", &"[REDACTED]")
            .finish()
    }
}

/// Signs trusted transaction identity for PostgreSQL verification.
#[derive(Clone)]
pub struct ContextIssuer {
    config: ContextIssuerConfig,
    signing_key: Arc<dyn ContextSigningKey>,
}

impl ContextIssuer {
    /// Creates an issuer using an injected Ed25519 signing capability.
    pub fn new(
        config: ContextIssuerConfig,
        signing_key: Arc<dyn ContextSigningKey>,
    ) -> Result<Self, ContextIssuerError> {
        if config.key_id != signing_key.key_id() {
            return Err(ContextIssuerError::KeyIdMismatch);
        }
        Ok(Self { config, signing_key })
    }

    fn issue(
        &self,
        scope: &Scope,
        caller: &ResolvedCaller,
        binding: &PgContextBinding,
    ) -> Result<String, ContextIssuerError> {
        let now = OffsetDateTime::now_utc();
        let lifetime = time::Duration::try_from(self.config.token_lifetime)
            .map_err(|_| ContextIssuerError::InvalidConfiguration)?;
        let expires_at = now
            .checked_add(lifetime)
            .ok_or(ContextIssuerError::InvalidConfiguration)?;

        let claims = ContextClaims {
            version: "1".to_owned(),
            tenant_id: scope.tenant_id().to_string(),
            principal_id: scope.principal().principal_id().to_string(),
            caller_id: caller.caller_id().to_string(),
            backend_pid: binding.backend_pid.to_string(),
            transaction_id: binding.transaction_id.clone(),
            database_oid: binding.database_oid.clone(),
            deployment_id: self.config.deployment_id.clone(),
            issuer: self.config.issuer.clone(),
            audience: self.config.audience.clone(),
            key_id: self.config.key_id.clone(),
            issued_at: now.format(&Rfc3339).map_err(|_| ContextIssuerError::TimestampFormat)?,
            expires_at: expires_at.format(&Rfc3339).map_err(|_| ContextIssuerError::TimestampFormat)?,
            nonce: Uuid::new_v4().to_string(),
        };

        let payload = serde_jcs::to_vec(&claims)
            .map_err(|_| ContextIssuerError::Canonicalization)?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(ContextIssuerError::PayloadTooLarge);
        }
        let signature = self.signing_key.sign(&payload)?;
        Ok(format!(
            "{TOKEN_PREFIX}:{}:{}",
            lowercase_hex(&payload),
            lowercase_hex(&signature)
        ))
    }
}

impl fmt::Debug for ContextIssuer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContextIssuer")
            .field("config", &self.config)
            .field("signing_key", &"[REDACTED]")
            .finish()
    }
}

#[derive(Serialize)]
struct ContextClaims {
    version: String,
    tenant_id: String,
    principal_id: String,
    caller_id: String,
    backend_pid: String,
    transaction_id: String,
    database_oid: String,
    deployment_id: String,
    issuer: String,
    audience: String,
    key_id: String,
    issued_at: String,
    expires_at: String,
    nonce: String,
}

struct PgContextBinding {
    backend_pid: i32,
    transaction_id: String,
    database_oid: String,
}

impl PgContextBinding {
    async fn read(transaction: &mut TenantTransaction) -> Result<Self, ContextTransactionError> {
        let row = sqlx::query_as::<_, (i32, String, String)>(
            "SELECT pg_backend_pid(), pg_current_xact_id()::text, (SELECT oid::text FROM pg_database WHERE datname = current_database())",
        )
        .fetch_one(transaction.connection())
        .await
        .map_err(ContextTransactionError::Database)?;
        Ok(Self {
            backend_pid: row.0,
            transaction_id: row.1,
            database_oid: row.2,
        })
    }
}

/// A signed-context transaction; alias preserves compatibility with existing
/// repository methods while those methods are migrated to the new entry point.
pub type ContextTransaction = TenantTransaction;

/// Begins a transaction, resolves direct Caller delegation, signs its PostgreSQL
/// binding and installs app.context_token transaction-locally.
///
/// The existing tenant/principal GUCs are temporary bootstrap selectors for the
/// current SQL delegation resolver only. They are not signed authorization
/// evidence and must not be used by production RLS after the SQL companion work.
pub async fn begin_context_transaction(
    pool: &sqlx::PgPool,
    scope: &Scope,
    requested_caller_id: CallerId,
    issuer: &ContextIssuer,
) -> Result<ContextTransaction, ContextTransactionError> {
    let mut transaction = begin_tenant_transaction(pool, scope)
        .await
        .map_err(ContextTransactionError::Database)?;

    let binding = PgContextBinding::read(&mut transaction).await?;
    let caller = resolve_delegated_caller(&mut transaction, requested_caller_id)
        .await
        .map_err(ContextTransactionError::CallerResolution)?;

    if !caller.is_bound_to(&transaction, scope) {
        return Err(ContextTransactionError::CallerTransactionMismatch);
    }

    // Signing is local and synchronous: no KMS/network operation is performed
    // while delegation row locks are held.
    let token = issuer
        .issue(scope, &caller, &binding)
        .map_err(ContextTransactionError::Issuer)?;

    sqlx::query("SELECT set_config('app.context_token', $1, true)")
        .bind(token)
        .execute(transaction.connection())
        .await
        .map_err(ContextTransactionError::Database)?;

    Ok(transaction)
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

/// Errors while creating a signed transaction context.
#[derive(Debug)]
pub enum ContextIssuerError {
    InvalidConfiguration,
    InvalidSigningKey,
    KeyIdMismatch,
    Canonicalization,
    TimestampFormat,
    PayloadTooLarge,
    SigningFailure(String),
}

impl fmt::Display for ContextIssuerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration => formatter.write_str("invalid ContextIssuer configuration"),
            Self::InvalidSigningKey => formatter.write_str("invalid Ed25519 signing seed"),
            Self::KeyIdMismatch => formatter.write_str("signing key ID does not match issuer configuration"),
            Self::Canonicalization => formatter.write_str("failed to canonicalize context claims"),
            Self::TimestampFormat => formatter.write_str("failed to format context timestamp"),
            Self::PayloadTooLarge => formatter.write_str("context payload exceeds 8 KiB"),
            Self::SigningFailure(reason) => write!(formatter, "Ed25519 signing failed: {reason}"),
        }
    }
}

impl std::error::Error for ContextIssuerError {}

/// Failure to begin or initialize a signed PostgreSQL context transaction.
#[derive(Debug)]
pub enum ContextTransactionError {
    CallerResolution(CallerResolutionError),
    CallerTransactionMismatch,
    Issuer(ContextIssuerError),
    Database(sqlx::Error),
}

impl fmt::Display for ContextTransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CallerResolution(error) => write!(formatter, "Caller resolution failed: {error}"),
            Self::CallerTransactionMismatch => formatter.write_str("resolved Caller is not bound to this transaction and Scope"),
            Self::Issuer(error) => write!(formatter, "ContextIssuer failed: {error}"),
            Self::Database(error) => write!(formatter, "context transaction database error: {error}"),
        }
    }
}

impl std::error::Error for ContextTransactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CallerResolution(error) => Some(error),
            Self::Issuer(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::CallerTransactionMismatch => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_hex_encodes_bytes_without_separators() {
        assert_eq!(lowercase_hex(&[0x00, 0x0a, 0xff]), "000aff");
    }

    #[test]
    fn issuer_configuration_rejects_empty_identity_and_long_lifetime() {
        assert!(ContextIssuerConfig::new("", "aud", "deployment", "key-1", Duration::from_secs(30)).is_err());
        assert!(ContextIssuerConfig::new("issuer", "aud", "deployment", "key-1", Duration::from_secs(61)).is_err());
        assert!(ContextIssuerConfig::new("issuer", "aud", "deployment", "key-1", Duration::ZERO).is_err());
    }

    #[test]
    fn ring_signer_produces_a_verifiable_ed25519_signature() {
        let signer = RingEd25519SigningKey::from_seed("key-1", &[7_u8; 32])
            .expect("32-byte Ed25519 seed must be accepted");
        let message = b"canonical transaction context";
        let signature = signer.sign(message).expect("signing must succeed");
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ED25519,
            signer.public_key(),
        )
        .verify(message, &signature)
        .expect("signature must verify");
        assert_ne!(signature, [0_u8; 64]);
        assert!(!format!("{signer:?}").contains("070707"));
    }

    #[test]
    fn issuer_rejects_a_mismatched_key_id() {
        let signer = RingEd25519SigningKey::from_seed("key-a", &[3_u8; 32])
            .expect("seed must be accepted");
        let config = ContextIssuerConfig::with_default_lifetime(
            "qembed-runtime",
            "postgres://deployment/db",
            "deployment-a",
            "key-b",
        )
        .expect("config must be valid");
        assert!(matches!(
            ContextIssuer::new(config, Arc::new(signer)),
            Err(ContextIssuerError::KeyIdMismatch)
        ));
    }
}
