//! Trusted reconciliation contract for ambiguous external tool outcomes.

use std::future::Future;

use serde_json::Value;

use crate::{
    ids::{CommandId, InvocationId, ToolExecutionId},
    tool_execution::ToolExecutionFencingToken,
};

/// Immutable checkpoint supplied to an external reconciliation adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolExecutionReconciliationRequest {
    execution_id: ToolExecutionId,
    invocation_id: InvocationId,
    command_id: CommandId,
    expected_version: u64,
    expected_fencing_token: ToolExecutionFencingToken,
}

impl ToolExecutionReconciliationRequest {
    #[must_use]
    pub const fn new(
        execution_id: ToolExecutionId,
        invocation_id: InvocationId,
        command_id: CommandId,
        expected_version: u64,
        expected_fencing_token: ToolExecutionFencingToken,
    ) -> Self {
        Self { execution_id, invocation_id, command_id, expected_version, expected_fencing_token }
    }

    #[must_use]
    pub const fn execution_id(&self) -> ToolExecutionId { self.execution_id }
    #[must_use]
    pub const fn invocation_id(&self) -> InvocationId { self.invocation_id }
    #[must_use]
    pub const fn command_id(&self) -> CommandId { self.command_id }
    #[must_use]
    pub const fn expected_version(&self) -> u64 { self.expected_version }
    #[must_use]
    pub const fn expected_fencing_token(&self) -> ToolExecutionFencingToken { self.expected_fencing_token }
}

/// Evidence-backed assessment of the external side effect.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolExecutionReconciliationOutcome {
    ConfirmedSucceeded { output: Option<Value>, output_ttl_seconds: Option<u32> },
    ConfirmedFailed { evidence: Option<Value> },
    /// The provider cannot yet prove either success or failure.
    Unresolved,
}

/// Adapter contract for checking an ambiguous command with its authoritative provider.
pub trait ToolExecutionReconciler: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    fn reconcile_tool_execution(
        &self,
        request: ToolExecutionReconciliationRequest,
    ) -> impl Future<Output = Result<ToolExecutionReconciliationOutcome, Self::Error>> + Send;
}

/// A terminal reconciliation result plus the exact checkpoint it was based on.
#[derive(Debug)]
pub struct ToolExecutionReconciliationCompletion {
    pub request: ToolExecutionReconciliationRequest,
    pub next: crate::tool_execution::ToolExecution,
    pub outcome: ToolExecutionReconciliationOutcome,
}
