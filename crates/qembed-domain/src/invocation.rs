//! Invocation entity and state machine.

use crate::ids::{CallerId, InvocationId, ObjectId, SnapshotId, TenantId};

/// Monotonic lease fencing token for an invocation.
///
/// A token is a value returned by the persistence/lease boundary. Constructing
/// this wrapper does not authorize a write: the repository must still compare
/// it with the current token stored in PostgreSQL.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FencingToken(u64);

impl FencingToken {
    pub const INITIAL: Self = Self(0);

    #[must_use]
    pub const fn from_value(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Invocation processing states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InvocationStatus {
    Received,
    ContextLoading,
    Processing,
    ExecutingTool,
    Verifying,
    /// Reserved for the M4 autonomous runtime.
    WaitingForInput,
    /// Reserved for the M4 autonomous runtime.
    WaitingForApproval,
    Completed,
    CompletedDegraded,
    Failed,
    Cancelled,
}

impl InvocationStatus {
    /// Returns whether the lifecycle permits a transition to `next`.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        if self.is_terminal() {
            return false;
        }

        if matches!(next, Self::Failed | Self::Cancelled) {
            return true;
        }

        matches!(
            (self, next),
            (Self::Received, Self::ContextLoading)
                | (Self::ContextLoading, Self::Processing)
                | (Self::Processing, Self::ExecutingTool)
                | (Self::ExecutingTool, Self::Processing)
                | (Self::Processing, Self::Verifying)
                | (Self::Verifying, Self::Completed)
                | (Self::Verifying, Self::CompletedDegraded)
        )
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::CompletedDegraded | Self::Failed | Self::Cancelled
        )
    }
}

/// A single interaction execution, independent from long-lived Tasks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    id: InvocationId,
    tenant_id: TenantId,
    object_id: ObjectId,
    caller_id: CallerId,
    snapshot_id: SnapshotId,
    status: InvocationStatus,
    version: u64,
    fencing_token: FencingToken,
}

impl Invocation {
    #[must_use]
    pub const fn new(
        id: InvocationId,
        tenant_id: TenantId,
        object_id: ObjectId,
        caller_id: CallerId,
        snapshot_id: SnapshotId,
    ) -> Self {
        Self {
            id,
            tenant_id,
            object_id,
            caller_id,
            snapshot_id,
            status: InvocationStatus::Received,
            version: 0,
            fencing_token: FencingToken::INITIAL,
        }
    }

    /// Reconstitutes an invocation from persisted state.
    #[must_use]
    pub const fn from_persisted(
        id: InvocationId,
        tenant_id: TenantId,
        object_id: ObjectId,
        caller_id: CallerId,
        snapshot_id: SnapshotId,
        status: InvocationStatus,
        version: u64,
        fencing_token: FencingToken,
    ) -> Self {
        Self {
            id,
            tenant_id,
            object_id,
            caller_id,
            snapshot_id,
            status,
            version,
            fencing_token,
        }
    }

    #[must_use]
    pub const fn id(&self) -> InvocationId {
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
    pub const fn caller_id(&self) -> CallerId {
        self.caller_id
    }

    #[must_use]
    pub const fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    #[must_use]
    pub const fn status(&self) -> InvocationStatus {
        self.status
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub const fn fencing_token(&self) -> FencingToken {
        self.fencing_token
    }

    /// Applies a state transition only for the currently held fencing token.
    pub fn transition_to(
        &mut self,
        next: InvocationStatus,
        token: FencingToken,
    ) -> Result<(), InvocationError> {
        if token != self.fencing_token {
            return Err(InvocationError::StaleFencingToken);
        }

        if !self.status.can_transition_to(next) {
            return Err(InvocationError::InvalidTransition {
                from: self.status,
                to: next,
            });
        }

        self.version = self
            .version
            .checked_add(1)
            .ok_or(InvocationError::VersionOverflow)?;
        self.status = next;
        Ok(())
    }

    /// Installs a newer fencing token after a successful lease reclaim.
    pub fn advance_fencing_token(&mut self, next: FencingToken) -> Result<(), InvocationError> {
        if self.status.is_terminal() {
            return Err(InvocationError::TerminalInvocationCannotBeReclaimed);
        }

        if next <= self.fencing_token {
            return Err(InvocationError::FencingTokenDidNotAdvance);
        }

        self.version = self
            .version
            .checked_add(1)
            .ok_or(InvocationError::VersionOverflow)?;
        self.fencing_token = next;
        Ok(())
    }
}

/// Invocation transition or concurrency error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvocationError {
    InvalidTransition {
        from: InvocationStatus,
        to: InvocationStatus,
    },
    StaleFencingToken,
    FencingTokenDidNotAdvance,
    TerminalInvocationCannotBeReclaimed,
    VersionOverflow,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation() -> Invocation {
        Invocation::new(
            InvocationId::new(),
            TenantId::new(),
            ObjectId::new(),
            CallerId::new(),
            SnapshotId::new(),
        )
    }

    #[test]
    fn invocation_transition_matrix_matches_contract() {
        use InvocationStatus as S;

        let states = [
            S::Received,
            S::ContextLoading,
            S::Processing,
            S::ExecutingTool,
            S::Verifying,
            S::WaitingForInput,
            S::WaitingForApproval,
            S::Completed,
            S::CompletedDegraded,
            S::Failed,
            S::Cancelled,
        ];

        for from in states {
            for to in states {
                let expected = if from.is_terminal() {
                    false
                } else if matches!(to, S::Failed | S::Cancelled) {
                    true
                } else {
                    matches!(
                        (from, to),
                        (S::Received, S::ContextLoading)
                            | (S::ContextLoading, S::Processing)
                            | (S::Processing, S::ExecutingTool)
                            | (S::ExecutingTool, S::Processing)
                            | (S::Processing, S::Verifying)
                            | (S::Verifying, S::Completed)
                            | (S::Verifying, S::CompletedDegraded)
                    )
                };

                assert_eq!(from.can_transition_to(to), expected, "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn invocation_requires_a_current_fencing_token() {
        let mut invocation = invocation();
        let stale = FencingToken::from_value(1);

        assert_eq!(
            invocation.transition_to(InvocationStatus::ContextLoading, stale),
            Err(InvocationError::StaleFencingToken)
        );
        assert_eq!(invocation.status(), InvocationStatus::Received);
        assert_eq!(invocation.version(), 0);
    }

    #[test]
    fn invocation_transitions_increment_optimistic_version() {
        let mut invocation = invocation();
        let token = FencingToken::INITIAL;

        invocation
            .transition_to(InvocationStatus::ContextLoading, token)
            .expect("received can load context");
        invocation
            .transition_to(InvocationStatus::Processing, token)
            .expect("context can be processed");

        assert_eq!(invocation.status(), InvocationStatus::Processing);
        assert_eq!(invocation.version(), 2);
    }

    #[test]
    fn terminal_invocation_cannot_be_reopened() {
        let mut invocation = invocation();
        let token = FencingToken::INITIAL;

        invocation
            .transition_to(InvocationStatus::Cancelled, token)
            .expect("any nonterminal state can be cancelled");

        assert!(invocation.status().is_terminal());
        assert_eq!(
            invocation.transition_to(InvocationStatus::Processing, token),
            Err(InvocationError::InvalidTransition {
                from: InvocationStatus::Cancelled,
                to: InvocationStatus::Processing,
            })
        );
    }

    #[test]
    fn lease_reclaim_advances_fencing_token_and_invalidates_old_worker() {
        let mut invocation = invocation();
        let old_token = FencingToken::INITIAL;
        let new_token = FencingToken::from_value(1);

        invocation
            .advance_fencing_token(new_token)
            .expect("reclaim installs a larger token");

        assert_eq!(
            invocation.transition_to(InvocationStatus::ContextLoading, old_token),
            Err(InvocationError::StaleFencingToken)
        );
        invocation
            .transition_to(InvocationStatus::ContextLoading, new_token)
            .expect("new lease holder can transition");
    }

    #[test]
    fn terminal_invocation_cannot_be_reclaimed() {
        let mut invocation = invocation();
        invocation
            .transition_to(InvocationStatus::Failed, FencingToken::INITIAL)
            .expect("received can fail");

        assert_eq!(
            invocation.advance_fencing_token(FencingToken::from_value(1)),
            Err(InvocationError::TerminalInvocationCannotBeReclaimed)
        );
    }

    #[test]
    fn arbitrary_transition_sequences_never_leave_terminal_states() {
        let states = [
            InvocationStatus::Received,
            InvocationStatus::ContextLoading,
            InvocationStatus::Processing,
            InvocationStatus::ExecutingTool,
            InvocationStatus::Verifying,
            InvocationStatus::WaitingForInput,
            InvocationStatus::WaitingForApproval,
            InvocationStatus::Completed,
            InvocationStatus::CompletedDegraded,
            InvocationStatus::Failed,
            InvocationStatus::Cancelled,
        ];

        // Exhaustively exercise every four-step candidate sequence from Received.
        // Invalid edges are ignored; every accepted path must preserve the terminal invariant.
        for a in states {
            for b in states {
                for c in states {
                    for d in states {
                        let mut current = InvocationStatus::Received;
                        for next in [a, b, c, d] {
                            if current.can_transition_to(next) {
                                current = next;
                            }
                            if current.is_terminal() {
                                assert!(
                                    !current.can_transition_to(InvocationStatus::Processing),
                                    "terminal state {current:?} must not transition"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
