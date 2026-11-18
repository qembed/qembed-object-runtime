//! Outbound ports required by domain-facing application services.
//!
//! These traits describe capabilities, not implementations. Adapter crates
//! implement them; the domain crate does not depend on SQLx, HTTP frameworks,
//! or provider SDKs.

use std::error::Error;
use std::future::Future;

use crate::chat::{ChatRequest, ChatResponse};
use crate::ids::{CallerId, InvocationId, TenantId};
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
    caller_id: CallerId,
    principal_kind: PrincipalKind,
}

impl AuthenticatedIdentity {
    /// Creates the result of a successful authentication operation.
    #[must_use]
    pub const fn new(
        tenant_id: TenantId,
        caller_id: CallerId,
        principal_kind: PrincipalKind,
    ) -> Self {
        Self {
            tenant_id,
            caller_id,
            principal_kind,
        }
    }

    #[must_use]
    pub const fn tenant_id(self) -> TenantId {
        self.tenant_id
    }

    #[must_use]
    pub const fn caller_id(self) -> CallerId {
        self.caller_id
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

/// Provider-neutral chat completion boundary.
///
/// Implementations adapt this contract to a concrete provider. SDK request,
/// response, error, and model types must remain inside the adapter crate.
pub trait ChatProvider: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn complete(
        &self,
        request: ChatRequest,
    ) -> impl Future<Output = Result<ChatResponse, Self::Error>> + Send;
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
                CallerId::new(),
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
        type Error = Infallible;

        fn complete(
            &self,
            _: ChatRequest,
        ) -> impl Future<Output = Result<ChatResponse, Self::Error>> + Send {
            ready(Ok(
                ChatResponse::new("test response", crate::chat::ChatFinishReason::Stop, None)
                    .expect("test response is valid"),
            ))
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
