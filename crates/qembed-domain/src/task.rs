//! Long-lived Task aggregate and immutable progress checkpoints.
//!
//! A Task is a durable parent for one or more Invocations. An Invocation is
//! one execution attempt; it does not become a Task merely by having multiple
//! steps. A Task may attach multiple Invocation IDs over its lifetime.
//!
//! Checkpoints record committed progress across those invocations. Sequence
//! and step indices are contiguous and monotonic, so a confirmed step cannot
//! be recorded a second time or skipped during resume. Persistence adapters
//! must enforce the same invariants atomically under the Task's concurrency
//! boundary; this in-memory aggregate is not a substitute for leases/CAS.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{InvocationId, TaskId, TenantId};

/// Durable lifecycle of a long-lived Task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    /// Whether this state permits a transition to the target state.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::Running | Self::Failed | Self::Cancelled)
                | (Self::Running, Self::Waiting | Self::Completed | Self::Failed | Self::Cancelled)
                | (Self::Waiting, Self::Running | Self::Failed | Self::Cancelled)
        )
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// A long-lived unit of work that may span multiple Invocations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    id: TaskId,
    tenant_id: TenantId,
    status: TaskStatus,
    version: u64,
    invocation_ids: Vec<InvocationId>,
    last_checkpoint_sequence: u64,
    last_step_index: Option<u32>,
}

impl Task {
    #[must_use]
    pub const fn new(id: TaskId, tenant_id: TenantId) -> Self {
        Self {
            id, tenant_id, status: TaskStatus::Pending, version: 0,
            invocation_ids: Vec::new(), last_checkpoint_sequence: 0, last_step_index: None,
        }
    }

    #[must_use]
    pub const fn id(&self) -> TaskId { self.id }
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId { self.tenant_id }
    #[must_use]
    pub const fn status(&self) -> TaskStatus { self.status }
    #[must_use]
    pub const fn version(&self) -> u64 { self.version }
    #[must_use]
    pub fn invocation_ids(&self) -> &[InvocationId] { &self.invocation_ids }
    #[must_use]
    pub const fn last_checkpoint_sequence(&self) -> u64 { self.last_checkpoint_sequence }
    #[must_use]
    pub const fn last_step_index(&self) -> Option<u32> { self.last_step_index }

    /// Adds an Invocation child exactly once. Invocations retain independent
    /// lifecycle and lease/fencing boundaries.
    pub fn attach_invocation(&mut self, invocation_id: InvocationId) -> Result<(), TaskError> {
        if self.status.is_terminal() { return Err(TaskError::TerminalTask); }
        if self.invocation_ids.contains(&invocation_id) {
            return Err(TaskError::InvocationAlreadyAttached);
        }
        self.bump_version()?;
        self.invocation_ids.push(invocation_id);
        Ok(())
    }

    pub fn transition_to(&mut self, next: TaskStatus) -> Result<(), TaskError> {
        if !self.status.can_transition_to(next) {
            return Err(TaskError::InvalidTransition { from: self.status, to: next });
        }
        self.bump_version()?;
        self.status = next;
        Ok(())
    }

    /// Commits the next checkpoint into this aggregate.
    ///
    /// Checkpoint sequence starts at 1; step index starts at 0. Both advance
    /// by exactly one. The Invocation must already be attached to this Task.
    pub fn record_checkpoint(&mut self, checkpoint: &Checkpoint) -> Result<(), TaskError> {
        if self.status != TaskStatus::Running { return Err(TaskError::TaskNotRunning); }
        if checkpoint.task_id != self.id { return Err(TaskError::CheckpointTaskMismatch); }
        if !self.invocation_ids.contains(&checkpoint.invocation_id) {
            return Err(TaskError::InvocationNotAttached);
        }
        let expected_sequence = self.last_checkpoint_sequence.checked_add(1)
            .ok_or(TaskError::SequenceOverflow)?;
        if checkpoint.sequence != expected_sequence {
            return Err(TaskError::InvalidCheckpointSequence {
                expected: expected_sequence, actual: checkpoint.sequence,
            });
        }
        let expected_step = match self.last_step_index {
            Some(index) => index.checked_add(1).ok_or(TaskError::SequenceOverflow)?,
            None => 0,
        };
        if checkpoint.step_index != expected_step {
            return Err(TaskError::InvalidStepIndex {
                expected: expected_step, actual: checkpoint.step_index,
            });
        }
        self.bump_version()?;
        self.last_checkpoint_sequence = checkpoint.sequence;
        self.last_step_index = Some(checkpoint.step_index);
        Ok(())
    }

    fn bump_version(&mut self) -> Result<(), TaskError> {
        self.version = self.version.checked_add(1).ok_or(TaskError::VersionOverflow)?;
        Ok(())
    }
}

/// Immutable, serializable progress record belonging to one Task and one of
/// its child Invocations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    task_id: TaskId,
    invocation_id: InvocationId,
    sequence: u64,
    step_index: u32,
    state: Value,
}

impl Checkpoint {
    #[must_use]
    pub fn new(task_id: TaskId, invocation_id: InvocationId, sequence: u64, step_index: u32, state: Value) -> Self {
        Self { task_id, invocation_id, sequence, step_index, state }
    }
    #[must_use]
    pub const fn task_id(&self) -> TaskId { self.task_id }
    #[must_use]
    pub const fn invocation_id(&self) -> InvocationId { self.invocation_id }
    #[must_use]
    pub const fn sequence(&self) -> u64 { self.sequence }
    #[must_use]
    pub const fn step_index(&self) -> u32 { self.step_index }
    #[must_use]
    pub fn state(&self) -> &Value { &self.state }
}

/// Task aggregate invariant error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskError {
    InvalidTransition { from: TaskStatus, to: TaskStatus },
    TerminalTask,
    TaskNotRunning,
    InvocationAlreadyAttached,
    InvocationNotAttached,
    CheckpointTaskMismatch,
    InvalidCheckpointSequence { expected: u64, actual: u64 },
    InvalidStepIndex { expected: u32, actual: u32 },
    SequenceOverflow,
    VersionOverflow,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn running_task() -> (Task, InvocationId) {
        let invocation_id = InvocationId::new();
        let mut task = Task::new(TaskId::new(), TenantId::new());
        task.attach_invocation(invocation_id).unwrap();
        task.transition_to(TaskStatus::Running).unwrap();
        (task, invocation_id)
    }

    fn checkpoint(task: &Task, invocation_id: InvocationId, sequence: u64, step: u32) -> Checkpoint {
        Checkpoint::new(task.id(), invocation_id, sequence, step, json!({"cursor": step}))
    }

    #[test]
    fn task_transition_matrix_matches_contract() {
        use TaskStatus as S;
        let states = [S::Pending, S::Running, S::Waiting, S::Completed, S::Failed, S::Cancelled];
        for from in states {
            for to in states {
                let expected = matches!(
                    (from, to),
                    (S::Pending, S::Running | S::Failed | S::Cancelled)
                        | (S::Running, S::Waiting | S::Completed | S::Failed | S::Cancelled)
                        | (S::Waiting, S::Running | S::Failed | S::Cancelled)
                );
                assert_eq!(from.can_transition_to(to), expected, "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn task_can_have_multiple_distinct_invocations() {
        let mut task = Task::new(TaskId::new(), TenantId::new());
        let first = InvocationId::new();
        let second = InvocationId::new();
        task.attach_invocation(first).unwrap();
        task.attach_invocation(second).unwrap();
        assert_eq!(task.invocation_ids(), &[first, second]);
        assert_eq!(task.attach_invocation(first), Err(TaskError::InvocationAlreadyAttached));
    }

    #[test]
    fn checkpoint_progress_continues_across_invocations_without_repeating_steps() {
        let (mut task, first_invocation) = running_task();
        let first = checkpoint(&task, first_invocation, 1, 0);
        task.record_checkpoint(&first).unwrap();
        let second_invocation = InvocationId::new();
        task.attach_invocation(second_invocation).unwrap();
        let second = checkpoint(&task, second_invocation, 2, 1);
        task.record_checkpoint(&second).unwrap();
        assert_eq!(task.last_checkpoint_sequence(), 2);
        assert_eq!(task.last_step_index(), Some(1));
    }

    #[test]
    fn checkpoint_rejects_repeated_or_skipped_sequence_and_step() {
        let (mut task, invocation_id) = running_task();
        let first = checkpoint(&task, invocation_id, 1, 0);
        task.record_checkpoint(&first).unwrap();
        let repeated_step = checkpoint(&task, invocation_id, 2, 0);
        assert_eq!(task.record_checkpoint(&repeated_step),
            Err(TaskError::InvalidStepIndex { expected: 1, actual: 0 }));
        let skipped_sequence = checkpoint(&task, invocation_id, 3, 2);
        assert_eq!(task.record_checkpoint(&skipped_sequence),
            Err(TaskError::InvalidCheckpointSequence { expected: 2, actual: 3 }));
    }

    #[test]
    fn checkpoint_requires_running_task_matching_task_and_attached_invocation() {
        let mut task = Task::new(TaskId::new(), TenantId::new());
        let invocation_id = InvocationId::new();
        let cp = checkpoint(&task, invocation_id, 1, 0);
        assert_eq!(task.record_checkpoint(&cp), Err(TaskError::TaskNotRunning));
        task.attach_invocation(invocation_id).unwrap();
        task.transition_to(TaskStatus::Running).unwrap();
        let foreign = Checkpoint::new(TaskId::new(), invocation_id, 1, 0, json!({}));
        assert_eq!(task.record_checkpoint(&foreign), Err(TaskError::CheckpointTaskMismatch));
        let unattached = checkpoint(&task, InvocationId::new(), 1, 0);
        assert_eq!(task.record_checkpoint(&unattached), Err(TaskError::InvocationNotAttached));
    }

    #[test]
    fn terminal_task_cannot_be_reopened_or_receive_invocations() {
        let mut task = Task::new(TaskId::new(), TenantId::new());
        task.transition_to(TaskStatus::Running).unwrap();
        task.transition_to(TaskStatus::Completed).unwrap();
        assert_eq!(task.transition_to(TaskStatus::Running), Err(TaskError::InvalidTransition {
            from: TaskStatus::Completed, to: TaskStatus::Running
        }));
        assert_eq!(task.attach_invocation(InvocationId::new()), Err(TaskError::TerminalTask));
    }

    #[test]
    fn task_and_checkpoint_serialize_and_deserialize_round_trip() {
        let (mut task, invocation_id) = running_task();
        let cp = checkpoint(&task, invocation_id, 1, 0);
        task.record_checkpoint(&cp).unwrap();
        let task_json = serde_json::to_string(&task).unwrap();
        let restored_task: Task = serde_json::from_str(&task_json).unwrap();
        assert_eq!(restored_task, task);
        let checkpoint_json = serde_json::to_string(&cp).unwrap();
        let restored_checkpoint: Checkpoint = serde_json::from_str(&checkpoint_json).unwrap();
        assert_eq!(restored_checkpoint, cp);
    }
}
