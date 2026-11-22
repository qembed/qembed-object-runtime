//! Trusted adapter contracts for the RT-050 KMCP application layer.
//!
//! These ports are synchronous. Implementations must not perform network I/O
//! while a PostgreSQL transaction or delegation row lock is held. Provision
//! keys before opening the registry transaction; then atomically persist
//! lifecycle state, idempotency, audit and outbox through KeyRegistry.

use crate::{LifecycleAction, LifecycleDecision, LifecycleState};

/// Authenticated administrative subject established by a trusted authorizer.
///
/// This is not a credential; never construct it from request DTO identity fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedAdmin {
    subject: String,
    deployment_id: String,
    permissions: Vec<AdminPermission>,
}

impl AuthorizedAdmin {
    /// Trust boundary: call only after authenticating claims and checking scope.
    pub fn from_verified_claims(
        subject: impl Into<String>,
        deployment_id: impl Into<String>,
        permissions: Vec<AdminPermission>,
    ) -> Result<Self, AuthorizationError> {
        let subject = subject.into();
        let deployment_id = deployment_id.into();
        if subject.trim().is_empty() || deployment_id.trim().is_empty() {
            return Err(AuthorizationError::InvalidVerifiedIdentity);
        }
        Ok(Self {
            subject,
            deployment_id,
            permissions,
        })
    }

    pub fn subject(&self) -> &str {
        &self.subject
    }

    pub fn deployment_id(&self) -> &str {
        &self.deployment_id
    }

    pub fn permits(&self, permission: AdminPermission) -> bool {
        self.permissions.contains(&permission)
    }
}

/// Explicit administrative capabilities; client-supplied role names are ignored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdminPermission {
    StageKey,
    ActivateKey,
    RetireKey,
    EmergencyRevokeKey,
    ReadKeyMetadata,
    ExportAudit,
}

/// The operation being authorized. Approval is a separate authorization call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdminOperation {
    Stage,
    Activate,
    Retire,
    EmergencyRevoke,
    ReadMetadata,
    ExportAudit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorizationError {
    Unauthenticated,
    PermissionDenied,
    DeploymentMismatch,
    InvalidVerifiedIdentity,
}

/// Opaque credential input. Implementations must never log its contents.
pub struct AdminCredential<'a> {
    pub opaque_token: &'a [u8],
}

/// Authenticates an operator and authorizes one operation in one deployment.
pub trait AdminAuthorizer: Send + Sync {
    fn authorize(
        &self,
        credential: &AdminCredential<'_>,
        deployment_id: &str,
        operation: AdminOperation,
    ) -> Result<AuthorizedAdmin, AuthorizationError>;
}

/// Public material returned after provider-side generation and proof of possession.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvisionedKey {
    pub key_id: String,
    pub public_key: [u8; 32],
    pub provider_reference: String,
    pub proof_signature: [u8; 64],
}

/// Administrative custody port; per-token signing remains ContextSigner.
pub trait KeyProvider: Send + Sync {
    fn provision_ed25519(
        &self,
        deployment_id: &str,
        key_id: &str,
        challenge: &[u8],
    ) -> Result<ProvisionedKey, ProviderError>;

    fn disable(&self, provider_reference: &str) -> Result<(), ProviderError>;

    fn destroy(&self, provider_reference: &str) -> Result<(), ProviderError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderError {
    Unavailable,
    UnsupportedAlgorithm,
    KeyAlreadyExists,
    KeyNotFound,
    ProofFailed,
    OperationRejected,
}

/// Persisted key snapshot. Version is the monotonic compare-and-set token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryKey {
    pub deployment_id: String,
    pub key_id: String,
    pub state: LifecycleState,
    pub version: u64,
    pub provider_reference: String,
    pub public_key: [u8; 32],
    pub verify_until_unix_seconds: Option<i64>,
}

/// Operation facts bound to both idempotency and immutable audit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationRecord {
    pub request_id: String,
    pub deployment_id: String,
    pub key_id: String,
    pub action: LifecycleAction,
    pub actor_subject: String,
    pub approver_subject: Option<String>,
    pub reason: String,
    pub incident_reference: Option<String>,
    pub decision: LifecycleDecision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    Applied { new_version: u64 },
    Replayed { original_version: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryError {
    NotFound,
    AlreadyExists,
    CompareAndSetConflict,
    IdempotencyConflict,
    Unavailable,
    ConstraintViolation,
}

/// PostgreSQL adapter must use narrowly scoped SECURITY DEFINER functions, not direct table DML.
pub trait KeyRegistry: Send + Sync {
    fn get_key(
        &self,
        deployment_id: &str,
        key_id: &str,
    ) -> Result<Option<RegistryKey>, RegistryError>;

    /// Atomically apply state, idempotency, append-only audit and outbox.
    /// expected_version=None is valid only for first-time staging.
    fn apply_operation(
        &self,
        expected_version: Option<u64>,
        operation: &OperationRecord,
    ) -> Result<ApplyOutcome, RegistryError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditEvent {
    pub event_id: String,
    pub deployment_id: String,
    pub event_type: String,
    pub canonical_payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditLease {
    pub event: AuditEvent,
    pub lease_token: String,
    pub attempt: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditExportError {
    Unavailable,
    InvalidReceipt,
    LeaseLost,
}

/// At-least-once delivery; sink must deduplicate by immutable event ID.
pub trait AuditOutboxExporter: Send + Sync {
    fn claim_batch(
        &self,
        limit: u32,
        lease_seconds: u32,
    ) -> Result<Vec<AuditLease>, AuditExportError>;

    fn acknowledge(
        &self,
        event_id: &str,
        lease_token: &str,
        sink_receipt: &str,
    ) -> Result<(), AuditExportError>;

    fn release_failed(
        &self,
        event_id: &str,
        lease_token: &str,
        sanitized_error: &str,
    ) -> Result<(), AuditExportError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_identity_requires_non_empty_subject_and_deployment() {
        assert_eq!(
            AuthorizedAdmin::from_verified_claims("", "prod-1", vec![AdminPermission::StageKey]),
            Err(AuthorizationError::InvalidVerifiedIdentity)
        );
        assert_eq!(
            AuthorizedAdmin::from_verified_claims("alice", " ", vec![AdminPermission::StageKey]),
            Err(AuthorizationError::InvalidVerifiedIdentity)
        );
    }

    #[test]
    fn permissions_are_explicit_and_deployment_scoped() {
        let admin = AuthorizedAdmin::from_verified_claims(
            "oidc:alice",
            "prod-eu-1",
            vec![AdminPermission::StageKey, AdminPermission::ReadKeyMetadata],
        )
        .unwrap();

        assert_eq!(admin.subject(), "oidc:alice");
        assert_eq!(admin.deployment_id(), "prod-eu-1");
        assert!(admin.permits(AdminPermission::StageKey));
        assert!(!admin.permits(AdminPermission::EmergencyRevokeKey));
    }

    #[test]
    fn provisioned_key_contains_public_material_only() {
        let key = ProvisionedKey {
            key_id: "ctx-2026-02".into(),
            public_key: [7; 32],
            provider_reference: "opaque-ref".into(),
            proof_signature: [9; 64],
        };
        assert_eq!(key.public_key.len(), 32);
        assert_eq!(key.proof_signature.len(), 64);
    }
}
