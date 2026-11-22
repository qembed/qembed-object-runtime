//! KMCP application orchestration over trusted adapter ports.
//!
//! This module is not an HTTP handler and does not authenticate credentials by
//! itself. It delegates that work to AdminAuthorizer, then enforces deployment
//! scope and explicit permissions before any provider or registry side effect.

use std::time::{SystemTime, UNIX_EPOCH};

use ring::{
    rand::{SecureRandom, SystemRandom},
    signature::{ED25519, UnparsedPublicKey},
};

use crate::{
    LifecycleAction, LifecycleError, LifecycleRequest, plan_transition,
    ports::{
        AdminAuthorizer, AdminCredential, AdminOperation, AdminPermission, ApplyOutcome,
        AuthorizationError, KeyProvider, KeyRegistry, OperationRecord, ProviderError,
        RegistryError, StagedKeyMaterial, StoredOperation,
    },
};

const MAX_CAS_ATTEMPTS: usize = 3;
const CHALLENGE_BYTES: usize = 32;

/// All caller-controlled operation data except authenticated identity.
///
/// Actor and approver subjects are always obtained by separate authorizer calls.
#[derive(Clone, Copy, Debug)]
pub struct LifecycleCommand<'a> {
    pub request_id: &'a str,
    pub deployment_id: &'a str,
    pub key_id: &'a str,
    pub reason: &'a str,
    pub incident_reference: Option<&'a str>,
    pub retirement_grace_seconds: Option<u32>,
}

/// A staged key operation requires no second operator.
#[derive(Clone, Copy, Debug)]
pub struct StageKeyCommand<'a> {
    pub lifecycle: LifecycleCommand<'a>,
}

/// Sanitized orchestration failures. No credentials, key bytes or provider
/// references are embedded in these values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KmcpError {
    ClockUnavailable,
    Authorization(AuthorizationError),
    Provider(ProviderError),
    Registry(RegistryError),
    Lifecycle(LifecycleError),
    EntropyUnavailable,
    ProofOfPossessionFailed,
    InvalidProviderResponse,
    IdempotencyConflict,
    DeploymentMismatch,
    RetryExhausted,
}

impl From<AuthorizationError> for KmcpError {
    fn from(value: AuthorizationError) -> Self {
        Self::Authorization(value)
    }
}

impl From<ProviderError> for KmcpError {
    fn from(value: ProviderError) -> Self {
        Self::Provider(value)
    }
}

impl From<RegistryError> for KmcpError {
    fn from(value: RegistryError) -> Self {
        Self::Registry(value)
    }
}

impl From<LifecycleError> for KmcpError {
    fn from(value: LifecycleError) -> Self {
        Self::Lifecycle(value)
    }
}

/// Trusted source of administrative policy time. Never take timestamps from
/// an HTTP/CLI request body.
pub trait Clock: Send + Sync {
    fn now_unix_seconds(&self) -> Result<i64, KmcpError>;
}

/// System clock implementation. Production composition should monitor clock
/// synchronization and enforce the deployment's skew budget.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_seconds(&self) -> Result<i64, KmcpError> {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| KmcpError::ClockUnavailable)?;
        i64::try_from(elapsed.as_secs()).map_err(|_| KmcpError::ClockUnavailable)
    }
}

/// Provider-neutral KMCP use-case service.
///
/// Implementations of the injected ports are part of the trusted computing
/// base. In particular, AdminAuthorizer must validate credentials and
/// deployment-scoped permissions; a client DTO is never an identity source.
pub struct KmcpService<'a> {
    authorizer: &'a dyn AdminAuthorizer,
    provider: &'a dyn KeyProvider,
    registry: &'a dyn KeyRegistry,
    clock: &'a dyn Clock,
}

impl<'a> KmcpService<'a> {
    pub fn new(
        authorizer: &'a dyn AdminAuthorizer,
        provider: &'a dyn KeyProvider,
        registry: &'a dyn KeyRegistry,
        clock: &'a dyn Clock,
    ) -> Self {
        Self {
            authorizer,
            provider,
            registry,
            clock,
        }
    }

    /// Provision an Ed25519 key, verify proof of possession, then atomically
    /// register it as staged with its idempotency record and audit outbox.
    ///
    /// The provider must make provisioning idempotent by deployment + request
    /// ID + key ID. This closes the retry window where provider creation
    /// succeeded but PostgreSQL registration failed.
    pub fn stage_key(
        &self,
        credential: &AdminCredential<'_>,
        command: StageKeyCommand<'_>,
    ) -> Result<ApplyOutcome, KmcpError> {
        let request = command.lifecycle;
        let actor = self.authorize(
            credential,
            request.deployment_id,
            AdminOperation::Stage,
            AdminPermission::StageKey,
        )?;

        if let Some(stored) = self
            .registry
            .get_operation(request.deployment_id, request.request_id)?
        {
            return replay_if_same_intent(
                stored,
                request,
                LifecycleAction::Stage,
                actor.subject(),
                None,
            );
        }

        if self
            .registry
            .get_key(request.deployment_id, request.key_id)?
            .is_some()
        {
            return Err(KmcpError::Registry(RegistryError::AlreadyExists));
        }

        let now_unix_seconds = self.clock.now_unix_seconds()?;
        let decision = plan_transition(
            None,
            LifecycleAction::Stage,
            lifecycle_request(request, actor.subject(), None, now_unix_seconds),
        )?;

        let challenge = random_challenge()?;
        let provisioned = self.provider.provision_ed25519(
            request.deployment_id,
            request.request_id,
            request.key_id,
            &challenge,
        )?;

        if provisioned.key_id != request.key_id || provisioned.provider_reference.trim().is_empty()
        {
            let _ = self.provider.disable(&provisioned.provider_reference);
            return Err(KmcpError::InvalidProviderResponse);
        }

        if UnparsedPublicKey::new(&ED25519, provisioned.public_key)
            .verify(&challenge, &provisioned.proof_signature)
            .is_err()
        {
            // Cleanup is best-effort here. Provider inventory reconciliation
            // remains mandatory because provider and PostgreSQL do not share
            // a distributed transaction.
            let _ = self.provider.disable(&provisioned.provider_reference);
            return Err(KmcpError::ProofOfPossessionFailed);
        }

        let operation = OperationRecord {
            request_id: request.request_id.to_owned(),
            deployment_id: request.deployment_id.to_owned(),
            key_id: request.key_id.to_owned(),
            action: LifecycleAction::Stage,
            actor_subject: actor.subject().to_owned(),
            approver_subject: None,
            reason: request.reason.to_owned(),
            incident_reference: None,
            staged_material: Some(StagedKeyMaterial {
                public_key: provisioned.public_key,
                provider_reference: provisioned.provider_reference,
            }),
            retirement_grace_seconds: None,
            decision,
        };

        self.registry
            .apply_operation(None, &operation)
            .map_err(KmcpError::from)
    }

    /// Activate a staged key. Actor and approver are authenticated separately.
    pub fn activate_key(
        &self,
        actor_credential: &AdminCredential<'_>,
        approver_credential: &AdminCredential<'_>,
        command: LifecycleCommand<'_>,
    ) -> Result<ApplyOutcome, KmcpError> {
        self.transition(
            actor_credential,
            Some(approver_credential),
            command,
            LifecycleAction::Activate,
            AdminOperation::Activate,
            AdminPermission::ActivateKey,
        )
    }

    /// Retire an active key with a bounded verification grace period.
    pub fn retire_key(
        &self,
        actor_credential: &AdminCredential<'_>,
        approver_credential: &AdminCredential<'_>,
        command: LifecycleCommand<'_>,
    ) -> Result<ApplyOutcome, KmcpError> {
        self.transition(
            actor_credential,
            Some(approver_credential),
            command,
            LifecycleAction::Retire,
            AdminOperation::Retire,
            AdminPermission::RetireKey,
        )
    }

    /// Emergency-revoke a key immediately. No approver or grace is accepted;
    /// policy requires an incident reference.
    pub fn revoke_key(
        &self,
        actor_credential: &AdminCredential<'_>,
        command: LifecycleCommand<'_>,
    ) -> Result<ApplyOutcome, KmcpError> {
        self.transition(
            actor_credential,
            None,
            command,
            LifecycleAction::Revoke,
            AdminOperation::EmergencyRevoke,
            AdminPermission::EmergencyRevokeKey,
        )
    }

    fn transition(
        &self,
        actor_credential: &AdminCredential<'_>,
        approver_credential: Option<&AdminCredential<'_>>,
        request: LifecycleCommand<'_>,
        action: LifecycleAction,
        operation: AdminOperation,
        permission: AdminPermission,
    ) -> Result<ApplyOutcome, KmcpError> {
        let actor = self.authorize(
            actor_credential,
            request.deployment_id,
            operation,
            permission,
        )?;
        let approver = match approver_credential {
            Some(credential) => {
                Some(self.authorize(credential, request.deployment_id, operation, permission)?)
            }
            None => None,
        };

        if let Some(stored) = self
            .registry
            .get_operation(request.deployment_id, request.request_id)?
        {
            return replay_if_same_intent(
                stored,
                request,
                action,
                actor.subject(),
                approver.as_ref().map(|admin| admin.subject()),
            );
        }

        let now_unix_seconds = self.clock.now_unix_seconds()?;
        for _ in 0..MAX_CAS_ATTEMPTS {
            let current = self
                .registry
                .get_key(request.deployment_id, request.key_id)?
                .ok_or(KmcpError::Registry(RegistryError::NotFound))?;
            let decision = plan_transition(
                Some(current.state),
                action,
                lifecycle_request(
                    request,
                    actor.subject(),
                    approver.as_ref().map(|admin| admin.subject()),
                    now_unix_seconds,
                ),
            )?;
            let record = OperationRecord {
                request_id: request.request_id.to_owned(),
                deployment_id: request.deployment_id.to_owned(),
                key_id: request.key_id.to_owned(),
                action,
                actor_subject: actor.subject().to_owned(),
                approver_subject: approver.as_ref().map(|admin| admin.subject().to_owned()),
                reason: request.reason.to_owned(),
                incident_reference: request.incident_reference.map(str::to_owned),
                staged_material: None,
                retirement_grace_seconds: request.retirement_grace_seconds,
                decision,
            };

            match self
                .registry
                .apply_operation(Some(current.version), &record)
            {
                Ok(outcome) => return Ok(outcome),
                Err(RegistryError::CompareAndSetConflict) => {
                    if let Some(stored) = self
                        .registry
                        .get_operation(request.deployment_id, request.request_id)?
                    {
                        return replay_if_same_intent(
                            stored,
                            request,
                            action,
                            actor.subject(),
                            approver.as_ref().map(|admin| admin.subject()),
                        );
                    }
                }
                Err(error) => return Err(KmcpError::Registry(error)),
            }
        }

        Err(KmcpError::RetryExhausted)
    }

    fn authorize(
        &self,
        credential: &AdminCredential<'_>,
        deployment_id: &str,
        operation: AdminOperation,
        permission: AdminPermission,
    ) -> Result<crate::AuthorizedAdmin, KmcpError> {
        let admin = self
            .authorizer
            .authorize(credential, deployment_id, operation)?;
        if admin.deployment_id() != deployment_id {
            return Err(KmcpError::DeploymentMismatch);
        }
        if !admin.permits(permission) {
            return Err(KmcpError::Authorization(
                AuthorizationError::PermissionDenied,
            ));
        }
        Ok(admin)
    }
}

fn lifecycle_request<'a>(
    request: LifecycleCommand<'a>,
    actor_subject: &'a str,
    approver_subject: Option<&'a str>,
    now_unix_seconds: i64,
) -> LifecycleRequest<'a> {
    LifecycleRequest {
        request_id: request.request_id,
        deployment_id: request.deployment_id,
        key_id: request.key_id,
        actor_subject,
        approver_subject,
        reason: request.reason,
        incident_reference: request.incident_reference,
        now_unix_seconds,
        retirement_grace_seconds: request.retirement_grace_seconds,
    }
}

fn random_challenge() -> Result<[u8; CHALLENGE_BYTES], KmcpError> {
    let mut challenge = [0_u8; CHALLENGE_BYTES];
    SystemRandom::new()
        .fill(&mut challenge)
        .map_err(|_| KmcpError::EntropyUnavailable)?;
    Ok(challenge)
}

fn replay_if_same_intent(
    stored: StoredOperation,
    request: LifecycleCommand<'_>,
    action: LifecycleAction,
    actor_subject: &str,
    approver_subject: Option<&str>,
) -> Result<ApplyOutcome, KmcpError> {
    let record = stored.record;
    let same = record.request_id == request.request_id
        && record.deployment_id == request.deployment_id
        && record.key_id == request.key_id
        && record.action == action
        && record.actor_subject == actor_subject
        && record.approver_subject.as_deref() == approver_subject
        && record.reason == request.reason
        && record.incident_reference.as_deref() == request.incident_reference
        && record.retirement_grace_seconds == request.retirement_grace_seconds;
    if same {
        Ok(stored.outcome)
    } else {
        Err(KmcpError::IdempotencyConflict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::ApplyOutcome;

    fn command() -> LifecycleCommand<'static> {
        LifecycleCommand {
            request_id: "request-1",
            deployment_id: "prod-eu-1",
            key_id: "ctx-2026-01",
            reason: "planned rotation",
            incident_reference: None,
            retirement_grace_seconds: Some(120),
        }
    }

    fn stored() -> StoredOperation {
        StoredOperation {
            record: OperationRecord {
                request_id: "request-1".into(),
                deployment_id: "prod-eu-1".into(),
                key_id: "ctx-2026-01".into(),
                action: LifecycleAction::Retire,
                actor_subject: "oidc:alice".into(),
                approver_subject: Some("oidc:bob".into()),
                reason: "planned rotation".into(),
                incident_reference: None,
                staged_material: None,
                retirement_grace_seconds: Some(120),
                decision: crate::LifecycleDecision {
                    next_state: crate::LifecycleState::Retired,
                    changed_at_unix_seconds: 1_800_000_000,
                    verify_until_unix_seconds: Some(1_800_000_120),
                },
            },
            outcome: ApplyOutcome::Applied { new_version: 3 },
        }
    }

    #[test]
    fn identical_retry_returns_original_durable_result() {
        let outcome = replay_if_same_intent(
            stored(),
            command(),
            LifecycleAction::Retire,
            "oidc:alice",
            Some("oidc:bob"),
        )
        .unwrap();
        assert_eq!(outcome, ApplyOutcome::Applied { new_version: 3 });
    }

    #[test]
    fn request_id_reuse_with_changed_grace_is_rejected() {
        let mut request = command();
        request.retirement_grace_seconds = Some(300);
        assert_eq!(
            replay_if_same_intent(
                stored(),
                request,
                LifecycleAction::Retire,
                "oidc:alice",
                Some("oidc:bob"),
            ),
            Err(KmcpError::IdempotencyConflict)
        );
    }

    #[test]
    fn request_id_reuse_with_changed_approver_is_rejected() {
        assert_eq!(
            replay_if_same_intent(
                stored(),
                command(),
                LifecycleAction::Retire,
                "oidc:alice",
                Some("oidc:mallory"),
            ),
            Err(KmcpError::IdempotencyConflict)
        );
    }
}
