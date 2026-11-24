//! Cryptographically signed PostgreSQL transaction context.
//!
//! The issuer signs canonical JSON with Ed25519. PostgreSQL receives only the
//! public verification material's key identifier and the signed token envelope;
//! private signing material never enters SQL parameters or GUCs.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use qembed_domain::anonymous::AnonymousContext;
use qembed_domain::ids::CallerId;
use qembed_domain::scope::Scope;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::caller::{
    AnonymousCallerResolutionError, CallerResolutionError, ResolvedAnonymousCaller, ResolvedCaller,
    resolve_anonymous_caller, resolve_delegated_caller,
};
use crate::{
    TenantTransaction, begin_anonymous_context_tenant_transaction, begin_context_tenant_transaction,
};

const TOKEN_PREFIX: &str = "qctx1";
const ANONYMOUS_TOKEN_PREFIX: &str = "qctx2";
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
        Self::new(
            issuer,
            audience,
            deployment_id,
            key_id,
            DEFAULT_TOKEN_LIFETIME,
        )
    }
}

/// Bounded local Ed25519 signing capability used by the ContextIssuer.
///
/// Implementations sign the exact supplied bytes and return a detached
/// 64-byte signature. The call is synchronous because it runs after PostgreSQL
/// transaction binding and Caller-delegation locks have been established.
/// Implementations MUST therefore use a bounded local capability (for example,
/// an in-process key or a local HSM/agent interface); they MUST NOT perform a
/// remote KMS/network request on the transaction path.
pub trait ContextSigner: Send + Sync {
    /// Returns the immutable public-key identifier associated with this signer.
    fn key_id(&self) -> &str;

    /// Signs exact canonical payload bytes and returns a detached signature.
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

impl ContextSigner for RingEd25519SigningKey {
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
    signing_key: Arc<dyn ContextSigner>,
}

impl ContextIssuer {
    /// Creates an issuer using an injected bounded local signing capability.
    ///
    /// The signer must not perform remote network/KMS calls: issuance occurs
    /// inside the PostgreSQL transaction after delegation locks are held.
    pub fn new(
        config: ContextIssuerConfig,
        signing_key: impl ContextSigner + 'static,
    ) -> Result<Self, ContextIssuerError> {
        if config.key_id != signing_key.key_id() {
            return Err(ContextIssuerError::KeyIdMismatch);
        }
        Ok(Self {
            config,
            signing_key: Arc::new(signing_key),
        })
    }

    fn issue(
        &self,
        scope: &Scope,
        caller_id: Option<CallerId>,
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
            caller_id: caller_id.map(|id| id.to_string()),
            backend_pid: binding.backend_pid.to_string(),
            transaction_id: binding.transaction_id.clone(),
            database_oid: binding.database_oid.clone(),
            deployment_id: self.config.deployment_id.clone(),
            issuer: self.config.issuer.clone(),
            audience: self.config.audience.clone(),
            key_id: self.config.key_id.clone(),
            issued_at: now
                .format(&Rfc3339)
                .map_err(|_| ContextIssuerError::TimestampFormat)?,
            expires_at: expires_at
                .format(&Rfc3339)
                .map_err(|_| ContextIssuerError::TimestampFormat)?,
            nonce: Uuid::new_v4().to_string(),
        };

        let payload =
            serde_jcs::to_vec(&claims).map_err(|_| ContextIssuerError::Canonicalization)?;
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

    /// Issues a version-2 Anonymous context. The exact claim set is distinct
    /// from qctx1: Principal is explicitly JSON null and Object/fingerprint/
    /// subject-kind are mandatory. No synthetic Principal ID is accepted.
    fn issue_anonymous(
        &self,
        context: &AnonymousContext,
        caller_id: Option<CallerId>,
        binding: &PgContextBinding,
    ) -> Result<String, ContextIssuerError> {
        let now = OffsetDateTime::now_utc();
        let lifetime = time::Duration::try_from(self.config.token_lifetime)
            .map_err(|_| ContextIssuerError::InvalidConfiguration)?;
        let expires_at = now
            .checked_add(lifetime)
            .ok_or(ContextIssuerError::InvalidConfiguration)?;

        let claims = AnonymousContextClaims {
            version: "2".to_owned(),
            tenant_id: context.tenant_id().to_string(),
            principal_id: None,
            caller_id: caller_id.map(|id| id.to_string()),
            subject_kind: "anonymous".to_owned(),
            object_id: context.object_id().to_string(),
            subject_fingerprint: lowercase_hex(&context.subject_fingerprint()),
            backend_pid: binding.backend_pid.to_string(),
            transaction_id: binding.transaction_id.clone(),
            database_oid: binding.database_oid.clone(),
            deployment_id: self.config.deployment_id.clone(),
            issuer: self.config.issuer.clone(),
            audience: self.config.audience.clone(),
            key_id: self.config.key_id.clone(),
            issued_at: now
                .format(&Rfc3339)
                .map_err(|_| ContextIssuerError::TimestampFormat)?,
            expires_at: expires_at
                .format(&Rfc3339)
                .map_err(|_| ContextIssuerError::TimestampFormat)?,
            nonce: Uuid::new_v4().to_string(),
        };

        let payload =
            serde_jcs::to_vec(&claims).map_err(|_| ContextIssuerError::Canonicalization)?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(ContextIssuerError::PayloadTooLarge);
        }
        let signature = self.signing_key.sign(&payload)?;
        Ok(format!(
            "{ANONYMOUS_TOKEN_PREFIX}:{}:{}",
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
    caller_id: Option<String>,
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

#[derive(Serialize)]
struct AnonymousContextClaims {
    version: String,
    tenant_id: String,
    principal_id: Option<String>,
    caller_id: Option<String>,
    subject_kind: String,
    object_id: String,
    subject_fingerprint: String,
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
/// Trusted tenant and Principal identifiers are passed explicitly to the
/// pre-token delegation resolver. This signed transaction does not install
/// legacy identity GUCs; only app.context_token carries effective SQL identity.
pub async fn begin_context_transaction(
    pool: &sqlx::PgPool,
    scope: &Scope,
    requested_caller_id: CallerId,
    issuer: &ContextIssuer,
) -> Result<ContextTransaction, ContextTransactionError> {
    begin_context_transaction_inner(pool, scope, Some(requested_caller_id), issuer)
        .await
        .map(|(transaction, _)| transaction)
}

/// Begins a Caller-scoped signed context and returns the exact database-resolved
/// Caller value while its delegation lock remains held by the open transaction.
pub async fn begin_caller_context_transaction(
    pool: &sqlx::PgPool,
    scope: &Scope,
    requested_caller_id: CallerId,
    issuer: &ContextIssuer,
) -> Result<(ContextTransaction, ResolvedCaller), ContextTransactionError> {
    let (transaction, caller) =
        begin_context_transaction_inner(pool, scope, Some(requested_caller_id), issuer).await?;
    caller
        .map(|caller| (transaction, caller))
        .ok_or(ContextTransactionError::CallerTransactionMismatch)
}

/// Begins a Principal-only signed context. The token contains JSON null for
/// caller_id; no Caller delegation is selected, resolved, or inferred.
pub async fn begin_principal_context_transaction(
    pool: &sqlx::PgPool,
    scope: &Scope,
    issuer: &ContextIssuer,
) -> Result<ContextTransaction, ContextTransactionError> {
    begin_context_transaction_inner(pool, scope, None, issuer)
        .await
        .map(|(transaction, _)| transaction)
}

/// Begins a read-only qctx2 context for an Anonymous Object policy check.
///
/// This issues the bootstrap token with a null Caller claim but deliberately
/// does not provision or resolve the Object's Anonymous Caller. It is suitable
/// for RLS-protected reads such as kill-switch evaluation that must happen
/// before any invocation-side provisioning.
pub async fn begin_anonymous_context_read_transaction(
    pool: &sqlx::PgPool,
    context: &AnonymousContext,
    issuer: &ContextIssuer,
) -> Result<ContextTransaction, ContextTransactionError> {
    let mut transaction = begin_anonymous_context_tenant_transaction(pool, context)
        .await
        .map_err(ContextTransactionError::Database)?;
    let binding = PgContextBinding::read(&mut transaction).await?;
    let bootstrap_token = issuer
        .issue_anonymous(context, None, &binding)
        .map_err(ContextTransactionError::Issuer)?;
    sqlx::query("SELECT set_config('app.context_token', $1, true)")
        .bind(bootstrap_token)
        .execute(transaction.connection())
        .await
        .map_err(ContextTransactionError::Database)?;
    Ok(transaction)
}

/// Begins a qctx2 transaction for one public Anonymous Object invocation.
///
/// PostgreSQL provisions or reuses the Object's stable Anonymous Caller under
/// a bootstrap token with a null Caller claim. A second token binds that Caller
/// to the same transaction, Object and server-derived fingerprint before the
/// resolver locks the Caller lifecycle row.
pub async fn begin_anonymous_context_transaction(
    pool: &sqlx::PgPool,
    context: &AnonymousContext,
    issuer: &ContextIssuer,
) -> Result<(ContextTransaction, ResolvedAnonymousCaller), ContextTransactionError> {
    let mut transaction = begin_anonymous_context_tenant_transaction(pool, context)
        .await
        .map_err(ContextTransactionError::Database)?;
    let binding = PgContextBinding::read(&mut transaction).await?;

    let bootstrap_token = issuer
        .issue_anonymous(context, None, &binding)
        .map_err(ContextTransactionError::Issuer)?;
    sqlx::query("SELECT set_config('app.context_token', $1, true)")
        .bind(bootstrap_token)
        .execute(transaction.connection())
        .await
        .map_err(ContextTransactionError::Database)?;

    let caller_uuid = sqlx::query_scalar::<_, Uuid>(
        "SELECT qembed_crypto.get_or_create_anonymous_caller($1::uuid, $2::uuid)",
    )
    .bind(context.tenant_id().to_string())
    .bind(context.object_id().to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(|error| match &error {
        sqlx::Error::Database(database_error)
            if database_error.code().as_deref() == Some("42501") =>
        {
            ContextTransactionError::AnonymousCallerResolution(
                AnonymousCallerResolutionError::NotPubliclyInvokable,
            )
        }
        _ => ContextTransactionError::Database(error),
    })?;
    let caller_id = CallerId::from_uuid(caller_uuid);

    let invocation_token = issuer
        .issue_anonymous(context, Some(caller_id), &binding)
        .map_err(ContextTransactionError::Issuer)?;
    sqlx::query("SELECT set_config('app.context_token', $1, true)")
        .bind(invocation_token)
        .execute(transaction.connection())
        .await
        .map_err(ContextTransactionError::Database)?;

    let caller = resolve_anonymous_caller(&mut transaction, context, caller_id)
        .await
        .map_err(ContextTransactionError::AnonymousCallerResolution)?;
    if !caller.is_bound_to(&transaction, context) {
        return Err(ContextTransactionError::CallerTransactionMismatch);
    }

    Ok((transaction, caller))
}

async fn begin_context_transaction_inner(
    pool: &sqlx::PgPool,
    scope: &Scope,
    requested_caller_id: Option<CallerId>,
    issuer: &ContextIssuer,
) -> Result<(ContextTransaction, Option<ResolvedCaller>), ContextTransactionError> {
    let mut transaction = begin_context_tenant_transaction(pool, scope)
        .await
        .map_err(ContextTransactionError::Database)?;

    let binding = PgContextBinding::read(&mut transaction).await?;
    let caller = if let Some(requested_caller_id) = requested_caller_id {
        let caller = resolve_delegated_caller(&mut transaction, requested_caller_id)
            .await
            .map_err(ContextTransactionError::CallerResolution)?;
        if !caller.is_bound_to(&transaction, scope) {
            return Err(ContextTransactionError::CallerTransactionMismatch);
        }
        Some(caller)
    } else {
        None
    };
    let caller_id = caller.map(ResolvedCaller::caller_id);

    // Signing is local and synchronous: no KMS/network operation is performed
    // while delegation row locks are held.
    let token = issuer
        .issue(scope, caller_id, &binding)
        .map_err(ContextTransactionError::Issuer)?;

    sqlx::query("SELECT set_config('app.context_token', $1, true)")
        .bind(token)
        .execute(transaction.connection())
        .await
        .map_err(ContextTransactionError::Database)?;

    Ok((transaction, caller))
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
            Self::InvalidConfiguration => {
                formatter.write_str("invalid ContextIssuer configuration")
            }
            Self::InvalidSigningKey => formatter.write_str("invalid Ed25519 signing seed"),
            Self::KeyIdMismatch => {
                formatter.write_str("signing key ID does not match issuer configuration")
            }
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
    AnonymousCallerResolution(AnonymousCallerResolutionError),
    CallerTransactionMismatch,
    Issuer(ContextIssuerError),
    Database(sqlx::Error),
}

impl fmt::Display for ContextTransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CallerResolution(error) => write!(formatter, "Caller resolution failed: {error}"),
            Self::AnonymousCallerResolution(error) => {
                write!(formatter, "Anonymous Caller resolution failed: {error}")
            }
            Self::CallerTransactionMismatch => {
                formatter.write_str("resolved Caller is not bound to this transaction and Scope")
            }
            Self::Issuer(error) => write!(formatter, "ContextIssuer failed: {error}"),
            Self::Database(error) => {
                write!(formatter, "context transaction database error: {error}")
            }
        }
    }
}

impl std::error::Error for ContextTransactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CallerResolution(error) => Some(error),
            Self::AnonymousCallerResolution(error) => Some(error),
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
        assert!(
            ContextIssuerConfig::new("", "aud", "deployment", "key-1", Duration::from_secs(30))
                .is_err()
        );
        assert!(
            ContextIssuerConfig::new(
                "issuer",
                "aud",
                "deployment",
                "key-1",
                Duration::from_secs(61)
            )
            .is_err()
        );
        assert!(
            ContextIssuerConfig::new("issuer", "aud", "deployment", "key-1", Duration::ZERO)
                .is_err()
        );
    }

    #[test]
    fn ring_signer_produces_a_verifiable_ed25519_signature() {
        let signer = RingEd25519SigningKey::from_seed("key-1", &[7_u8; 32])
            .expect("32-byte Ed25519 seed must be accepted");
        let message = b"canonical transaction context";
        let signature = signer.sign(message).expect("signing must succeed");
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &signer.public_key())
            .verify(message, &signature)
            .expect("signature must verify");
        assert_ne!(signature, [0_u8; 64]);
        assert!(!format!("{signer:?}").contains("070707"));
    }

    struct AlternateLocalSigner(RingEd25519SigningKey);

    impl ContextSigner for AlternateLocalSigner {
        fn key_id(&self) -> &str {
            self.0.key_id()
        }

        fn sign(&self, message: &[u8]) -> Result<[u8; 64], ContextIssuerError> {
            self.0.sign(message)
        }
    }

    #[test]
    fn issuer_accepts_an_alternate_bounded_local_signer() {
        let signer = AlternateLocalSigner(
            RingEd25519SigningKey::from_seed("key-1", &[9_u8; 32]).expect("seed must be accepted"),
        );
        let config = ContextIssuerConfig::with_default_lifetime(
            "qembed-runtime",
            "postgres://deployment/db",
            "deployment-a",
            "key-1",
        )
        .expect("config must be valid");

        assert!(ContextIssuer::new(config, signer).is_ok());
    }

    #[test]
    fn qctx2_anonymous_claims_have_exact_shape_and_never_mint_a_principal() {
        use qembed_domain::ids::{ObjectId, TenantId};

        let signer = RingEd25519SigningKey::from_seed("key-v2", &[11_u8; 32])
            .expect("seed must be accepted");
        let config = ContextIssuerConfig::with_default_lifetime(
            "qembed-runtime",
            "postgres://deployment/db",
            "deployment-v2",
            "key-v2",
        )
        .expect("config must be valid");
        let issuer = ContextIssuer::new(config, signer).expect("issuer should be valid");
        let fingerprint = [0x5a; 32];
        let context = AnonymousContext::from_server_fingerprint(
            TenantId::new(),
            ObjectId::new(),
            fingerprint,
        )
        .expect("trusted context should be valid");
        let binding = PgContextBinding {
            backend_pid: 123,
            transaction_id: "456".to_owned(),
            database_oid: "789".to_owned(),
        };

        for caller_id in [None, Some(CallerId::new())] {
            let token = issuer
                .issue_anonymous(&context, caller_id, &binding)
                .expect("qctx2 token should be issued");
            assert!(token.starts_with("qctx2:"));

            let payload_hex = token.split(':').nth(1).expect("payload segment exists");
            let payload = decode_test_hex(payload_hex);
            let claims: serde_json::Value =
                serde_json::from_slice(&payload).expect("payload should be JSON");
            let object = claims.as_object().expect("claims should be an object");

            assert_eq!(object.len(), 17, "qctx2 must use its exact claim set");
            assert_eq!(claims["version"], "2");
            assert_eq!(claims["tenant_id"], context.tenant_id().to_string());
            assert_eq!(claims["principal_id"], serde_json::Value::Null);
            assert_eq!(
                claims["caller_id"],
                caller_id
                    .map(|id| serde_json::Value::String(id.to_string()))
                    .unwrap_or(serde_json::Value::Null)
            );
            assert_eq!(claims["subject_kind"], "anonymous");
            assert_eq!(claims["object_id"], context.object_id().to_string());
            assert_eq!(claims["subject_fingerprint"], lowercase_hex(&fingerprint));
            assert_eq!(claims["backend_pid"], "123");
            assert_eq!(claims["transaction_id"], "456");
            assert_eq!(claims["database_oid"], "789");
        }
    }

    fn decode_test_hex(value: &str) -> Vec<u8> {
        let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
        assert!(remainder.is_empty(), "hex input must have an even length");
        pairs
            .iter()
            .map(|pair| {
                let high = (pair[0] as char).to_digit(16).expect("hex digit") as u8;
                let low = (pair[1] as char).to_digit(16).expect("hex digit") as u8;
                (high << 4) | low
            })
            .collect()
    }
}
