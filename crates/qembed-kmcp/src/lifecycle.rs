//! Pure policy for the monotonic RT-050 verification-key lifecycle.
//!
//! This module is intentionally deterministic. It decides whether a requested
//! state transition is allowed; it does not perform database writes, call a
//! key provider, authenticate an operator, or emit audit events.

use std::fmt;

/// Production lifecycle states. Transitions are monotonic; a key is never
/// reactivated and a key identifier is never reused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleState {
    /// Public key registered but not eligible to sign or verify.
    Staged,
    /// Key may sign and verify new context tokens.
    Active,
    /// Key may no longer sign; pre-retirement tokens have bounded grace.
    Retired,
    /// Key is immediately rejected, without a grace period.
    Revoked,
}

/// Administrative operation requested against one deployment/key pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleAction {
    /// Register a newly provisioned public key as non-verifying.
    Stage,
    /// Approve a staged key for issuance and verification.
    Activate,
    /// Stop issuance and retain bounded verification for existing tokens.
    Retire,
    /// Immediately reject the key, including tokens inside a retirement grace.
    Revoke,
}

/// Authenticated operation facts supplied by the KMCP application layer.
///
/// These fields are data, not proof of authentication. The service MUST build
/// this value only after its injected authorization adapter has authenticated
/// the actor and checked deployment-scoped permissions. In particular, actor
/// and approver subjects must never be copied from an untrusted request DTO.
#[derive(Clone, Copy, Debug)]
pub struct LifecycleRequest<'a> {
    /// Stable idempotency key for this administrative operation.
    pub request_id: &'a str,
    /// Stable deployment identity; key IDs are unique within this scope.
    pub deployment_id: &'a str,
    /// Immutable key identifier; identifiers are never reused.
    pub key_id: &'a str,
    /// Authenticated operator or break-glass subject.
    pub actor_subject: &'a str,
    /// Separately authenticated approver for planned activation/retirement.
    pub approver_subject: Option<&'a str>,
    /// Human-readable operational reason.
    pub reason: &'a str,
    /// Required incident/ticket reference for emergency revocation.
    pub incident_reference: Option<&'a str>,
    /// Trusted KMCP clock, expressed as Unix seconds.
    pub now_unix_seconds: i64,
    /// Requested retirement grace; required only for retirement.
    pub retirement_grace_seconds: Option<u32>,
}

/// Result of a successfully validated transition.
///
/// The persistence adapter must apply this decision with a compare-and-set
/// predicate against the state observed by the caller. A stale decision must
/// fail as a conflict and be recomputed from fresh registry state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecycleDecision {
    /// State to persist.
    pub next_state: LifecycleState,
    /// Effective transition time from the trusted KMCP clock.
    pub changed_at_unix_seconds: i64,
    /// Exclusive verification deadline for retirement; absent otherwise.
    pub verify_until_unix_seconds: Option<i64>,
}

/// Policy violation. Errors intentionally avoid embedding secrets or raw
/// credentials so they are safe to expose as sanitized administrative errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleError {
    MissingRequestId,
    MissingDeploymentId,
    MissingKeyId,
    MissingActor,
    MissingReason,
    MissingApprover,
    ActorAndApproverMustDiffer,
    UnexpectedApprover,
    MissingIncidentReference,
    UnexpectedIncidentReference,
    MissingRetirementGrace,
    UnexpectedRetirementGrace,
    RetirementGraceOutOfRange,
    RetirementDeadlineOverflow,
    KeyAlreadyExists,
    KeyDoesNotExist,
    InvalidTransition,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::MissingRequestId => "request ID is required",
            Self::MissingDeploymentId => "deployment ID is required",
            Self::MissingKeyId => "key ID is required",
            Self::MissingActor => "authenticated actor is required",
            Self::MissingReason => "operation reason is required",
            Self::MissingApprover => "a distinct authenticated approver is required",
            Self::ActorAndApproverMustDiffer => "actor and approver must be different subjects",
            Self::UnexpectedApprover => "approver is not valid for this operation",
            Self::MissingIncidentReference => "incident reference is required for revocation",
            Self::UnexpectedIncidentReference => {
                "incident reference is not valid for this operation"
            }
            Self::MissingRetirementGrace => "retirement grace is required",
            Self::UnexpectedRetirementGrace => "retirement grace is not valid for this operation",
            Self::RetirementGraceOutOfRange => {
                "retirement grace must be between 1 and 3600 seconds"
            }
            Self::RetirementDeadlineOverflow => {
                "retirement deadline is outside the supported timestamp range"
            }
            Self::KeyAlreadyExists => "key identifier already exists in this deployment",
            Self::KeyDoesNotExist => "key identifier does not exist in this deployment",
            Self::InvalidTransition => "requested key lifecycle transition is not allowed",
        };
        f.write_str(message)
    }
}

impl std::error::Error for LifecycleError {}

/// Validate one requested transition and return the exact state to persist.
///
/// Allowed transitions:
///
/// - absent -> staged
/// - staged -> active
/// - staged -> revoked
/// - active -> retired
/// - active -> revoked
/// - retired -> revoked
///
/// No other transition is legal. In particular, activation requires two
/// distinct authenticated subjects, retirement requires two distinct
/// authenticated subjects and a bounded grace period, and emergency revocation
/// requires an incident reference but deliberately has no approval grace.
pub fn plan_transition(
    current: Option<LifecycleState>,
    action: LifecycleAction,
    request: LifecycleRequest<'_>,
) -> Result<LifecycleDecision, LifecycleError> {
    require_non_empty(request.request_id, LifecycleError::MissingRequestId)?;
    require_non_empty(request.deployment_id, LifecycleError::MissingDeploymentId)?;
    require_non_empty(request.key_id, LifecycleError::MissingKeyId)?;
    require_non_empty(request.actor_subject, LifecycleError::MissingActor)?;
    require_non_empty(request.reason, LifecycleError::MissingReason)?;

    let (next_state, verify_until) = match action {
        LifecycleAction::Stage => {
            require_no_approver(request.approver_subject)?;
            require_no_incident(request.incident_reference)?;
            require_no_grace(request.retirement_grace_seconds)?;
            if current.is_some() {
                return Err(LifecycleError::KeyAlreadyExists);
            }
            (LifecycleState::Staged, None)
        }
        LifecycleAction::Activate => {
            require_distinct_approvers(request.actor_subject, request.approver_subject)?;
            require_no_incident(request.incident_reference)?;
            require_no_grace(request.retirement_grace_seconds)?;
            if current != Some(LifecycleState::Staged) {
                return Err(if current.is_none() {
                    LifecycleError::KeyDoesNotExist
                } else {
                    LifecycleError::InvalidTransition
                });
            }
            (LifecycleState::Active, None)
        }
        LifecycleAction::Retire => {
            require_distinct_approvers(request.actor_subject, request.approver_subject)?;
            require_no_incident(request.incident_reference)?;
            let grace = request
                .retirement_grace_seconds
                .ok_or(LifecycleError::MissingRetirementGrace)?;
            if !(1..=3600).contains(&grace) {
                return Err(LifecycleError::RetirementGraceOutOfRange);
            }
            if current != Some(LifecycleState::Active) {
                return Err(if current.is_none() {
                    LifecycleError::KeyDoesNotExist
                } else {
                    LifecycleError::InvalidTransition
                });
            }
            let deadline = request
                .now_unix_seconds
                .checked_add(i64::from(grace))
                .ok_or(LifecycleError::RetirementDeadlineOverflow)?;
            (LifecycleState::Retired, Some(deadline))
        }
        LifecycleAction::Revoke => {
            require_no_approver(request.approver_subject)?;
            require_non_empty(
                request.incident_reference.unwrap_or_default(),
                LifecycleError::MissingIncidentReference,
            )?;
            require_no_grace(request.retirement_grace_seconds)?;
            match current {
                Some(LifecycleState::Staged | LifecycleState::Active | LifecycleState::Retired) => {
                }
                None => return Err(LifecycleError::KeyDoesNotExist),
                Some(LifecycleState::Revoked) => return Err(LifecycleError::InvalidTransition),
            }
            (LifecycleState::Revoked, None)
        }
    };

    Ok(LifecycleDecision {
        next_state,
        changed_at_unix_seconds: request.now_unix_seconds,
        verify_until_unix_seconds: verify_until,
    })
}

fn require_non_empty(value: &str, error: LifecycleError) -> Result<(), LifecycleError> {
    if value.trim().is_empty() {
        Err(error)
    } else {
        Ok(())
    }
}

fn require_no_approver(value: Option<&str>) -> Result<(), LifecycleError> {
    if value.is_some() {
        Err(LifecycleError::UnexpectedApprover)
    } else {
        Ok(())
    }
}

fn require_no_incident(value: Option<&str>) -> Result<(), LifecycleError> {
    if value.is_some() {
        Err(LifecycleError::UnexpectedIncidentReference)
    } else {
        Ok(())
    }
}

fn require_no_grace(value: Option<u32>) -> Result<(), LifecycleError> {
    if value.is_some() {
        Err(LifecycleError::UnexpectedRetirementGrace)
    } else {
        Ok(())
    }
}

fn require_distinct_approvers(actor: &str, approver: Option<&str>) -> Result<(), LifecycleError> {
    let approver = approver.ok_or(LifecycleError::MissingApprover)?;
    require_non_empty(approver, LifecycleError::MissingApprover)?;
    if actor == approver {
        return Err(LifecycleError::ActorAndApproverMustDiffer);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> LifecycleRequest<'static> {
        LifecycleRequest {
            request_id: "req-001",
            deployment_id: "prod-eu-1",
            key_id: "ctx-2026-01",
            actor_subject: "operator:alice",
            approver_subject: None,
            reason: "routine key lifecycle test",
            incident_reference: None,
            now_unix_seconds: 1_800_000_000,
            retirement_grace_seconds: None,
        }
    }

    #[test]
    fn stage_creates_non_verifying_key_without_approval() {
        let decision = plan_transition(None, LifecycleAction::Stage, request()).unwrap();
        assert_eq!(decision.next_state, LifecycleState::Staged);
        assert_eq!(decision.verify_until_unix_seconds, None);
    }

    #[test]
    fn activation_requires_two_distinct_subjects() {
        let mut input = request();
        input.approver_subject = Some("operator:alice");
        assert_eq!(
            plan_transition(
                Some(LifecycleState::Staged),
                LifecycleAction::Activate,
                input
            ),
            Err(LifecycleError::ActorAndApproverMustDiffer)
        );

        input.approver_subject = Some("approver:bob");
        let decision = plan_transition(
            Some(LifecycleState::Staged),
            LifecycleAction::Activate,
            input,
        )
        .unwrap();
        assert_eq!(decision.next_state, LifecycleState::Active);
    }

    #[test]
    fn retirement_has_bounded_exclusive_deadline() {
        let mut input = request();
        input.approver_subject = Some("approver:bob");
        input.retirement_grace_seconds = Some(120);
        let decision =
            plan_transition(Some(LifecycleState::Active), LifecycleAction::Retire, input).unwrap();
        assert_eq!(decision.next_state, LifecycleState::Retired);
        assert_eq!(decision.verify_until_unix_seconds, Some(1_800_000_120));
    }

    #[test]
    fn retirement_rejects_zero_and_more_than_one_hour() {
        let mut input = request();
        input.approver_subject = Some("approver:bob");
        for grace in [0, 3601] {
            input.retirement_grace_seconds = Some(grace);
            assert_eq!(
                plan_transition(Some(LifecycleState::Active), LifecycleAction::Retire, input),
                Err(LifecycleError::RetirementGraceOutOfRange)
            );
        }
    }

    #[test]
    fn retirement_deadline_overflow_is_rejected() {
        let mut input = request();
        input.approver_subject = Some("approver:bob");
        input.retirement_grace_seconds = Some(120);
        input.now_unix_seconds = i64::MAX;
        assert_eq!(
            plan_transition(Some(LifecycleState::Active), LifecycleAction::Retire, input),
            Err(LifecycleError::RetirementDeadlineOverflow)
        );
    }

    #[test]
    fn emergency_revocation_requires_incident_and_has_no_grace() {
        let mut input = request();
        assert_eq!(
            plan_transition(Some(LifecycleState::Active), LifecycleAction::Revoke, input),
            Err(LifecycleError::MissingIncidentReference)
        );

        input.incident_reference = Some("INC-4821");
        let decision = plan_transition(
            Some(LifecycleState::Retired),
            LifecycleAction::Revoke,
            input,
        )
        .unwrap();
        assert_eq!(decision.next_state, LifecycleState::Revoked);
        assert_eq!(decision.verify_until_unix_seconds, None);
    }

    #[test]
    fn lifecycle_is_monotonic_and_key_ids_cannot_be_reused() {
        assert_eq!(
            plan_transition(
                Some(LifecycleState::Active),
                LifecycleAction::Stage,
                request()
            ),
            Err(LifecycleError::KeyAlreadyExists)
        );
        assert_eq!(
            plan_transition(Some(LifecycleState::Retired), LifecycleAction::Activate, {
                let mut input = request();
                input.approver_subject = Some("approver:bob");
                input
            }),
            Err(LifecycleError::InvalidTransition)
        );
        assert_eq!(
            plan_transition(Some(LifecycleState::Revoked), LifecycleAction::Revoke, {
                let mut input = request();
                input.incident_reference = Some("INC-4821");
                input
            }),
            Err(LifecycleError::InvalidTransition)
        );
    }

    #[test]
    fn unknown_key_cannot_be_activated_or_revoked() {
        let mut input = request();
        input.approver_subject = Some("approver:bob");
        assert_eq!(
            plan_transition(None, LifecycleAction::Activate, input),
            Err(LifecycleError::KeyDoesNotExist)
        );
        input.approver_subject = None;
        input.incident_reference = Some("INC-4821");
        assert_eq!(
            plan_transition(None, LifecycleAction::Revoke, input),
            Err(LifecycleError::KeyDoesNotExist)
        );
    }

    #[test]
    fn unexpected_metadata_is_rejected() {
        let mut input = request();
        input.approver_subject = Some("approver:bob");
        assert_eq!(
            plan_transition(None, LifecycleAction::Stage, input),
            Err(LifecycleError::UnexpectedApprover)
        );
    }
}
