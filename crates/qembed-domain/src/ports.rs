//! Outbound ports required by domain-facing application services.
//!
//! These traits describe capabilities, not implementations. Adapter crates
//! implement them; the domain crate does not depend on SQLx, HTTP frameworks,
//! or provider SDKs.

use std::error::Error;
use std::future::Future;

use crate::chat::{
    ChatCapabilities, ChatProviderError, ChatRequest, ChatResponse, ProviderRequestContext,
};
use crate::ids::{InvocationId, PrincipalId, TenantId};
use crate::invocation::{FencingToken, Invocation};
use crate::principal::PrincipalKind;
use crate::scope::Scope;

/// Identity returned by a trusted authentication adapter after credential
/// verification.
///
/// This is an adapter result, not an unforgeable authorization capability.
/// Only the authentication adapter wired by the application composition root
/// may be trusted to produce it. Domain authentication logic is responsible
/// for turning this verified result into a Principal and Scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthenticatedIdentity {
    tenant_id: TenantId,
    principal_id: PrincipalId,
    principal_kind: PrincipalKind,
}

impl AuthenticatedIdentity {
    /// Creates the result of a successful authentication operation.
    #[must_use]
    pub const fn new(
        tenant_id: TenantId,
        principal_id: PrincipalId,
        principal_kind: PrincipalKind,
    ) -> Self {
        Self {
            tenant_id,
            principal_id,
            principal_kind,
        }
    }

    #[must_use]
    pub const fn tenant_id(self) -> TenantId {
        self.tenant_id
    }

    #[must_use]
    pub const fn principal_id(self) -> PrincipalId {
        self.principal_id
    }

    #[must_use]
    pub const fn principal_kind(self) -> PrincipalKind {
        self.principal_kind
    }
}

/// Verifies credentials and returns the identity established by that check.
///
/// The application must wire only a trusted verifier. An HTTP request DTO
/// must never be used directly as an authenticated identity.
pub trait AuthenticationPort: Send + Sync {
    type Credentials: Send;
    type Error: Error + Send + Sync + 'static;

    fn authenticate(
        &self,
        credentials: Self::Credentials,
    ) -> impl Future<Output = Result<AuthenticatedIdentity, Self::Error>> + Send;
}

/// Persistence boundary for invocation execution state.
///
/// Implementations must scope every operation to the supplied trusted Scope.
/// The compare-and-set operation must atomically validate the tenant,
/// invocation ID, expected optimistic version, and fencing token.
/// Ok(false) means the persisted state no longer matches those expectations;
/// it is not a transient success and must not be treated as a completed write.
pub trait ExecutionStore: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn load_invocation(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
    ) -> impl Future<Output = Result<Option<Invocation>, Self::Error>> + Send;

    fn compare_and_set_invocation(
        &self,
        scope: &Scope,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
}

/// Durable persistence operations required by the minimal invocation worker.
///
/// The worker stores its provider response before entering Verifying. This
/// makes a restart from Verifying independent of another provider request.
/// Lease acquisition/reclaim is deliberately a separate RUST-038 concern.
pub trait InvocationWorkStore: ExecutionStore {
    fn load_user_input(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
    ) -> impl Future<Output = Result<Option<String>, Self::Error>> + Send;

    fn load_assistant_output(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
    ) -> impl Future<Output = Result<Option<String>, Self::Error>> + Send;

    /// Atomically persist the assistant response and transition Processing → Verifying.
    fn persist_assistant_output(
        &self,
        scope: &Scope,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
        content: String,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;

    /// Transition Verifying → Completed after the response is already durable.
    fn complete_invocation(
        &self,
        scope: &Scope,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;

    /// Persist a terminal failure and its stable, non-sensitive error code.
    fn fail_invocation(
        &self,
        scope: &Scope,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
        error_code: &'static str,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
}

/// Read-only persistence boundary for the client-visible invocation projection.
/// Implementations must use the trusted Scope and return None for both a missing
/// invocation and an invocation belonging to another tenant.
pub trait InvocationReadStore: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn get_invocation(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
    ) -> impl Future<
        Output = Result<Option<crate::invocation_read::InvocationReadModel>, Self::Error>,
    > + Send;
}

/// Provider-neutral chat completion boundary.
///
/// Implementations adapt this contract to a concrete provider. SDK request,
/// response, error, and model types must remain inside the adapter crate.
pub trait ChatProvider: Send + Sync {
    /// Returns the explicit capabilities configured for an application-owned
    /// model routing key. None means the route is not configured.
    ///
    /// This lookup must be local and side-effect free: capability negotiation
    /// happens before complete, so unsupported requests never cause provider
    /// network I/O. Adapters must not infer capabilities from the key string.
    fn capabilities(&self, model_key: &str) -> Option<ChatCapabilities>;

    fn complete(
        &self,
        request: ChatRequest,
    ) -> impl Future<Output = Result<ChatResponse, ChatProviderError>> + Send;

    /// Completes a request with stable operation context.
    ///
    /// The default preserves compatibility for providers without idempotency
    /// support. An adapter advertising idempotency-key support must override
    /// this method and forward the supplied key unchanged to its provider.
    fn complete_with_context(
        &self,
        request: ChatRequest,
        _context: ProviderRequestContext,
    ) -> impl Future<Output = Result<ChatResponse, ChatProviderError>> + Send {
        self.complete(request)
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::future::ready;

    use super::*;

    struct TestAuthentication;

    impl AuthenticationPort for TestAuthentication {
        type Credentials = ();
        type Error = Infallible;
        fn authenticate(
            &self,
            (): Self::Credentials,
        ) -> impl Future<Output = Result<AuthenticatedIdentity, Self::Error>> + Send {
            ready(Ok(AuthenticatedIdentity::new(
                TenantId::new(),
                PrincipalId::new(),
                PrincipalKind::Authenticated,
            )))
        }
    }

    struct TestExecutionStore;

    impl ExecutionStore for TestExecutionStore {
        type Error = Infallible;

        fn load_invocation(
            &self,
            _: &Scope,
            _: InvocationId,
        ) -> impl Future<Output = Result<Option<Invocation>, Self::Error>> + Send {
            ready(Ok(None))
        }

        fn compare_and_set_invocation(
            &self,
            _: &Scope,
            _: u64,
            _: FencingToken,
            _: Invocation,
        ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
            ready(Ok(true))
        }
    }

    struct TestProvider;

    impl ChatProvider for TestProvider {
        fn capabilities(&self, _: &str) -> Option<ChatCapabilities> {
            Some(ChatCapabilities::default())
        }

        fn complete(
            &self,
            _: ChatRequest,
        ) -> impl Future<Output = Result<ChatResponse, ChatProviderError>> + Send {
            ready(Ok(ChatResponse::new(
                "test response",
                crate::chat::ChatFinishReason::Stop,
                None,
            )
            .expect("test response is valid")))
        }
    }

    fn assert_authentication_port<T: AuthenticationPort>(_: &T) {}
    fn assert_execution_store<T: ExecutionStore>(_: &T) {}
    fn assert_chat_provider<T: ChatProvider>(_: &T) {}

    // Test doubles type-check without a database, network, executor, or SDK.
    #[test]
    fn test_doubles_implement_all_domain_ports() {
        assert_authentication_port(&TestAuthentication);
        assert_execution_store(&TestExecutionStore);
        assert_chat_provider(&TestProvider);
    }
}
