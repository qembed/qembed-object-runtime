//! ToolExecution entity and independent lease-fenced state machine.

use crate::ids::{CommandId, InvocationId, ObjectId, TenantId, ToolExecutionId};

/// Monotonic fencing token owned exclusively by one ToolExecution.
///
/// It is intentionally a different Rust type from Invocation's fencing token:
///
/// ```compile_fail
/// use qembed_domain::invocation::FencingToken;
/// use qembed_domain::tool_execution::ToolExecutionFencingToken;
///
/// let invocation_token = FencingToken::INITIAL;
/// let _: ToolExecutionFencingToken = invocation_token;
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolExecutionFencingToken(u64);

impl ToolExecutionFencingToken {
    /// Initial token before the first ToolExecution lease is claimed.
    pub const INITIAL: Self = Self(0);

    /// Wraps a token returned by the persistence boundary.
    #[must_use]
    pub const fn from_value(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric token value for a conditional database write.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Lifecycle states of one authorized tool command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolExecutionStatus {
    /// Policy authorized the command; no worker owns it yet.
    Authorized,
    /// A worker holds the independent ToolExecution lease.
    Executing,
    /// The tool result was durably accepted.
    Succeeded,
    /// The tool execution failed with a known outcome.
    Failed,
    /// The lease expired while the external outcome may be ambiguous.
    Unknown,
}

impl ToolExecutionStatus {
    /// Returns whether the SQL contract permits this lifecycle edge.
    ///
    /// Unknown-to-terminal edges additionally require a matching reconciliation
    /// result; the dedicated reconciliation API is introduced separately.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        if self.is_terminal() {
            return false;
        }

        matches!(
            (self, next),
            (Self::Authorized, Self::Executing)
                | (Self::Authorized, Self::Failed)
                | (Self::Executing, Self::Succeeded)
                | (Self::Executing, Self::Failed)
                | (Self::Executing, Self::Unknown)
                | (Self::Unknown, Self::Succeeded)
                | (Self::Unknown, Self::Failed)
        )
    }

    /// Returns whether the execution has a final, reconciled outcome.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

/// Persisted concurrency checkpoint for one ToolExecution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolExecutionCheckpoint {
    status: ToolExecutionStatus,
    version: u64,
    fencing_token: ToolExecutionFencingToken,
    lease_owner: Option<String>,
}

impl ToolExecutionCheckpoint {
    /// Creates a checkpoint returned by the tenant-scoped persistence adapter.
    #[must_use]
    pub const fn new(
        status: ToolExecutionStatus,
        version: u64,
        fencing_token: ToolExecutionFencingToken,
        lease_owner: Option<String>,
    ) -> Self {
        Self {
            status,
            version,
            fencing_token,
            lease_owner,
        }
    }

    #[must_use]
    pub const fn status(&self) -> ToolExecutionStatus {
        self.status
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub const fn fencing_token(&self) -> ToolExecutionFencingToken {
        self.fencing_token
    }

    #[must_use]
    pub fn lease_owner(&self) -> Option<&str> {
        self.lease_owner.as_deref()
    }
}

/// One command execution, with its own optimistic version and lease fence.
///
/// Invocation identity is retained for provenance only. Invocation's lease,
/// version and fencing token never authorize a ToolExecution mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolExecution {
    id: ToolExecutionId,
    tenant_id: TenantId,
    object_id: ObjectId,
    invocation_id: InvocationId,
    command_id: CommandId,
    status: ToolExecutionStatus,
    version: u64,
    fencing_token: ToolExecutionFencingToken,
    lease_owner: Option<String>,
}

impl ToolExecution {
    /// Creates a newly authorized command with independent version/token zero.
    #[must_use]
    pub const fn new(
        id: ToolExecutionId,
        tenant_id: TenantId,
        object_id: ObjectId,
        invocation_id: InvocationId,
        command_id: CommandId,
    ) -> Self {
        Self {
            id,
            tenant_id,
            object_id,
            invocation_id,
            command_id,
            status: ToolExecutionStatus::Authorized,
            version: 0,
            fencing_token: ToolExecutionFencingToken::INITIAL,
            lease_owner: None,
        }
    }

    /// Reconstitutes a ToolExecution from its tenant-scoped persisted row.
    #[must_use]
    pub fn from_persisted(
        id: ToolExecutionId,
        tenant_id: TenantId,
        object_id: ObjectId,
        invocation_id: InvocationId,
        command_id: CommandId,
        checkpoint: ToolExecutionCheckpoint,
    ) -> Self {
        Self {
            id,
            tenant_id,
            object_id,
            invocation_id,
            command_id,
            status: checkpoint.status,
            version: checkpoint.version,
            fencing_token: checkpoint.fencing_token,
            lease_owner: checkpoint.lease_owner,
        }
    }

    #[must_use]
    pub const fn id(&self) -> ToolExecutionId {
        self.id
    }

    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    #[must_use]
    pub const fn object_id(&self) -> ObjectId {
        self.object_id
    }

    #[must_use]
    pub const fn invocation_id(&self) -> InvocationId {
        self.invocation_id
    }

    #[must_use]
    pub const fn command_id(&self) -> CommandId {
        self.command_id
    }

    #[must_use]
    pub const fn status(&self) -> ToolExecutionStatus {
        self.status
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub const fn fencing_token(&self) -> ToolExecutionFencingToken {
        self.fencing_token
    }

    #[must_use]
    pub fn lease_owner(&self) -> Option<&str> {
        self.lease_owner.as_deref()
    }

    /// Claims an Authorized execution for one worker.
    ///
    /// The claim advances this ToolExecution's version and fencing token once.
    /// PostgreSQL must perform the matching atomic CAS and establish lease expiry.
    pub fn claim(&mut self, owner: &str) -> Result<(), ToolExecutionError> {
        if owner.trim().is_empty() {
            return Err(ToolExecutionError::InvalidLeaseOwner);
        }
        self.ensure_transition(ToolExecutionStatus::Executing)?;

        let next_version = self
            .version
            .checked_add(1)
            .ok_or(ToolExecutionError::VersionOverflow)?;
        let next_token = self
            .fencing_token
            .value()
            .checked_add(1)
            .ok_or(ToolExecutionError::FencingTokenOverflow)?;

        self.version = next_version;
        self.fencing_token = ToolExecutionFencingToken::from_value(next_token);
        self.lease_owner = Some(owner.to_owned());
        self.status = ToolExecutionStatus::Executing;
        Ok(())
    }

    /// Verifies that a worker still presents this execution's owner and fence.
    ///
    /// Lease expiry is authoritative in PostgreSQL and is checked atomically by
    /// the adapter in addition to this in-memory identity check.
    pub fn ensure_lease_owner(
        &self,
        owner: &str,
        fencing_token: ToolExecutionFencingToken,
    ) -> Result<(), ToolExecutionError> {
        if self.status != ToolExecutionStatus::Executing
            || self.lease_owner.as_deref() != Some(owner)
        {
            return Err(ToolExecutionError::LeaseNotOwned);
        }
        if fencing_token != self.fencing_token {
            return Err(ToolExecutionError::StaleFencingToken);
        }
        Ok(())
    }

    /// Applies a known result under the current ToolExecution lease.
    ///
    /// A result transition advances only this execution's version. The fencing
    /// token is retained as audit evidence and the lease owner is cleared.
    pub fn complete(
        &mut self,
        owner: &str,
        fencing_token: ToolExecutionFencingToken,
        outcome: ToolExecutionOutcome,
    ) -> Result<(), ToolExecutionError> {
        self.ensure_lease_owner(owner, fencing_token)?;
        let next = match outcome {
            ToolExecutionOutcome::Succeeded => ToolExecutionStatus::Succeeded,
            ToolExecutionOutcome::Failed => ToolExecutionStatus::Failed,
        };
        self.ensure_transition(next)?;

        self.version = self
            .version
            .checked_add(1)
            .ok_or(ToolExecutionError::VersionOverflow)?;
        self.status = next;
        self.lease_owner = None;
        Ok(())
    }

    fn ensure_transition(&self, next: ToolExecutionStatus) -> Result<(), ToolExecutionError> {
        if self.status.can_transition_to(next) {
            Ok(())
        } else {
            Err(ToolExecutionError::InvalidTransition {
                from: self.status,
                to: next,
            })
        }
    }
}

/// A known outcome written by the worker holding the ToolExecution lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolExecutionOutcome {
    Succeeded,
    Failed,
}

/// Invalid ToolExecution transition or stale lease identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolExecutionError {
    InvalidTransition {
        from: ToolExecutionStatus,
        to: ToolExecutionStatus,
    },
    InvalidLeaseOwner,
    LeaseNotOwned,
    StaleFencingToken,
    VersionOverflow,
    FencingTokenOverflow,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn execution() -> ToolExecution {
        ToolExecution::new(
            ToolExecutionId::new(),
            TenantId::new(),
            ObjectId::new(),
            InvocationId::new(),
            CommandId::new(),
        )
    }

    #[test]
    fn transition_matrix_matches_the_tool_execution_contract() {
        use ToolExecutionStatus as S;

        let states = [
            S::Authorized,
            S::Executing,
            S::Succeeded,
            S::Failed,
            S::Unknown,
        ];

        for from in states {
            for to in states {
                let expected = matches!(
                    (from, to),
                    (S::Authorized, S::Executing)
                        | (S::Authorized, S::Failed)
                        | (S::Executing, S::Succeeded)
                        | (S::Executing, S::Failed)
                        | (S::Executing, S::Unknown)
                        | (S::Unknown, S::Succeeded)
                        | (S::Unknown, S::Failed)
                );
                assert_eq!(from.can_transition_to(to), expected, "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn claim_advances_only_the_tool_execution_owned_counters() {
        let mut execution = execution();
        assert_eq!(execution.version(), 0);
        assert_eq!(
            execution.fencing_token(),
            ToolExecutionFencingToken::INITIAL
        );

        execution
            .claim("tool-worker-a")
            .expect("authorized row can be claimed");

        assert_eq!(execution.status(), ToolExecutionStatus::Executing);
        assert_eq!(execution.version(), 1);
        assert_eq!(execution.fencing_token().value(), 1);
        assert_eq!(execution.lease_owner(), Some("tool-worker-a"));
    }

    #[test]
    fn a_different_tool_execution_fence_is_rejected() {
        let mut execution = execution();
        execution.claim("tool-worker-a").expect("claim succeeds");

        assert_eq!(
            execution.ensure_lease_owner("tool-worker-a", ToolExecutionFencingToken::from_value(2)),
            Err(ToolExecutionError::StaleFencingToken)
        );
    }

    #[test]
    fn heartbeat_validation_does_not_advance_version_or_fencing_token() {
        let mut execution = execution();
        execution.claim("tool-worker-a").expect("claim succeeds");
        let version = execution.version();
        let token = execution.fencing_token();

        execution
            .ensure_lease_owner("tool-worker-a", token)
            .expect("the current owner and fence are valid");

        assert_eq!(execution.version(), version);
        assert_eq!(execution.fencing_token(), token);
    }

    #[test]
    fn stale_owner_or_fence_cannot_complete_the_execution() {
        let mut execution = execution();
        execution.claim("tool-worker-a").expect("claim succeeds");

        assert_eq!(
            execution.complete(
                "tool-worker-b",
                execution.fencing_token(),
                ToolExecutionOutcome::Succeeded,
            ),
            Err(ToolExecutionError::LeaseNotOwned)
        );
        assert_eq!(
            execution.complete(
                "tool-worker-a",
                ToolExecutionFencingToken::INITIAL,
                ToolExecutionOutcome::Succeeded,
            ),
            Err(ToolExecutionError::StaleFencingToken)
        );
        assert_eq!(execution.status(), ToolExecutionStatus::Executing);
        assert_eq!(execution.version(), 1);
    }

    #[test]
    fn completion_advances_version_but_retains_the_claim_fence() {
        let mut execution = execution();
        execution.claim("tool-worker-a").expect("claim succeeds");
        let token = execution.fencing_token();

        execution
            .complete("tool-worker-a", token, ToolExecutionOutcome::Succeeded)
            .expect("current lease owner can commit the result");

        assert_eq!(execution.status(), ToolExecutionStatus::Succeeded);
        assert_eq!(execution.version(), 2);
        assert_eq!(execution.fencing_token(), token);
        assert_eq!(execution.lease_owner(), None);
    }

    #[test]
    fn empty_lease_owner_is_rejected() {
        let mut execution = execution();

        assert_eq!(
            execution.claim("  "),
            Err(ToolExecutionError::InvalidLeaseOwner)
        );
        assert_eq!(execution.status(), ToolExecutionStatus::Authorized);
        assert_eq!(execution.version(), 0);
    }

    #[test]
    fn terminal_execution_cannot_be_claimed_or_reopened() {
        let mut execution = execution();
        execution.claim("tool-worker-a").expect("claim succeeds");
        let token = execution.fencing_token();
        execution
            .complete("tool-worker-a", token, ToolExecutionOutcome::Failed)
            .expect("failure is terminal");

        assert!(execution.status().is_terminal());
        assert_eq!(
            execution.claim("tool-worker-b"),
            Err(ToolExecutionError::InvalidTransition {
                from: ToolExecutionStatus::Failed,
                to: ToolExecutionStatus::Executing,
            })
        );
    }
}
