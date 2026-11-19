//! Lease-aware invocation persistence contract.

use std::future::Future;

use crate::ids::InvocationId;
use crate::invocation::{FencingToken, Invocation};
use crate::ports::InvocationWorkStore;
use crate::scope::Scope;

/// Persistence boundary for exclusively owned invocation work.
///
/// Every mutating method must atomically require the trusted tenant scope,
/// exact owner, unexpired lease, fencing token, optimistic version and
/// expected lifecycle state. A false result means the caller no longer owns
/// the invocation and must stop processing it.
pub trait InvocationLeaseStore: InvocationWorkStore {
    /// Claim an invocation with no current lease, or reclaim an expired lease.
    ///
    /// Initial claim sets owner/expiry/heartbeat without changing version or
    /// token. Reclaim increments both version and token exactly once.
    fn claim_invocation(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
        owner: &str,
        lease_seconds: u32,
    ) -> impl Future<Output = Result<Option<Invocation>, Self::Error>> + Send;

    /// Extend an unexpired lease held by this owner and token.
    ///
    /// An expired lease cannot be revived by heartbeat; it must be reclaimed.
    fn heartbeat_invocation(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
        owner: &str,
        fencing_token: FencingToken,
        lease_seconds: u32,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;

    /// Compare-and-set a nonterminal lifecycle transition under a live lease.
    fn compare_and_set_owned(
        &self,
        scope: &Scope,
        owner: &str,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;

    /// Persist assistant output and Processing → Verifying under one live lease.
    fn persist_assistant_output_owned(
        &self,
        scope: &Scope,
        owner: &str,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
        content: String,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;

    /// Load the durable provider-call state using the trusted tenant scope.
    fn load_provider_call_attempt(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
    ) -> impl Future<
        Output = Result<Option<crate::provider_call_attempt::ProviderCallAttempt>, Self::Error>,
    > + Send;

    /// Commit the conservative pre-send marker under the current live lease.
    /// A false result means the caller must not contact the provider.
    fn mark_provider_may_have_been_sent_owned(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
        owner: &str,
        expected_version: u64,
        expected_fencing_token: FencingToken,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;

    /// Persist an ambiguous provider outcome without releasing its reservation.
    fn mark_provider_outcome_unknown_owned(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
        owner: &str,
        expected_version: u64,
        expected_fencing_token: FencingToken,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;

    /// Atomically persist assistant output, normalized usage, ResponseReceived,
    /// and Processing → Verifying under the live lease.
    fn persist_provider_response_owned(
        &self,
        scope: &Scope,
        owner: &str,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
        content: String,
        usage: Option<crate::chat::TokenUsage>,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;

    /// Complete Verifying → Completed and release the lease atomically.
    fn complete_invocation_owned(
        &self,
        scope: &Scope,
        owner: &str,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;

    /// Persist a stable provider failure and release the lease atomically.
    fn fail_invocation_owned(
        &self,
        scope: &Scope,
        owner: &str,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
        error_code: &'static str,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
}
