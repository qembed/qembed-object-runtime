//! Lease-aware ToolExecution persistence contract.

use std::future::Future;

use serde_json::Value;

use crate::ids::ToolExecutionId;
use crate::scope::Scope;
use crate::tool_execution::{ToolExecution, ToolExecutionFencingToken};

/// Inputs for an atomic ToolExecution result write.
#[derive(Debug)]
pub struct ToolExecutionCompletion {
    pub execution_id: ToolExecutionId,
    pub owner: String,
    pub expected_version: u64,
    pub expected_fencing_token: ToolExecutionFencingToken,
    pub next: ToolExecution,
    pub output: Option<Value>,
    pub output_ttl_seconds: Option<u32>,
}

/// Persistence boundary for one independently leased tool command.
///
/// Every mutation is scoped to the trusted tenant and must atomically compare
/// ToolExecution's own version, fencing token, owner and unexpired lease.
/// Invocation's lease/version/token are deliberately absent from this contract.
pub trait ToolExecutionLeaseStore: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Loads a ToolExecution visible in the trusted tenant Scope.
    fn load_tool_execution(
        &self,
        scope: &Scope,
        execution_id: ToolExecutionId,
    ) -> impl Future<Output = Result<Option<ToolExecution>, Self::Error>> + Send;

    /// Atomically claims an Authorized execution and creates its independent lease.
    ///
    /// The initial claim advances the ToolExecution version and fencing token
    /// exactly once. A concurrent loser receives None and must not run the tool.
    fn claim_tool_execution(
        &self,
        scope: &Scope,
        execution_id: ToolExecutionId,
        owner: &str,
        lease_seconds: u32,
    ) -> impl Future<Output = Result<Option<ToolExecution>, Self::Error>> + Send;

    /// Extends a currently live lease without advancing version or fencing token.
    ///
    /// A stale version/token, different owner, expired lease or non-Executing
    /// state returns false. Heartbeat cannot revive an expired lease.
    fn heartbeat_tool_execution(
        &self,
        scope: &Scope,
        execution_id: ToolExecutionId,
        owner: &str,
        expected_version: u64,
        expected_fencing_token: ToolExecutionFencingToken,
        lease_seconds: u32,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;

    /// Marks an Executing row Unknown only after its PostgreSQL lease has expired.
    ///
    /// This operation is a conditional transition, not a claim/retry operation.
    /// It compares the ToolExecution-owned version and fencing token and uses
    /// the database clock as the authority for lease expiry. A live lease,
    /// stale checkpoint or already-transitioned row returns false.
    fn mark_tool_execution_unknown(
        &self,
        scope: &Scope,
        execution_id: ToolExecutionId,
        expected_version: u64,
        expected_fencing_token: ToolExecutionFencingToken,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;

    /// Commits a known result under the current ToolExecution lease.
    ///
    /// The caller must first apply the domain state transition to `next`.
    /// Successful output, when present, must have a positive retention TTL.
    /// The SQL adapter verifies owner, unexpired lease, expected version and
    /// ToolExecution fencing token in the same UPDATE that writes the result.
    fn complete_tool_execution(
        &self,
        scope: &Scope,
        completion: ToolExecutionCompletion,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
}
