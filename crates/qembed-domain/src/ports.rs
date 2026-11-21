//! Outbound ports required by domain-facing application services.
//!
//! These traits describe capabilities, not implementations. Adapter crates
//! implement them; the domain crate does not depend on SQLx, HTTP frameworks,
//! or provider SDKs.

use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::caller::InvocationPool;
use crate::chat::{
    ChatCapabilities, ChatProviderError, ChatRequest, ChatResponse, ProviderRequestContext,
};
use crate::ids::{
    CallerId, InvocationId, ObjectId, ObjectVersionId, PrincipalId, TenantId, TraceId,
};
use crate::invocation::{FencingToken, Invocation};
use crate::object::ObjectState;
use crate::object_validation::ValidationEvidence;
use crate::principal::PrincipalKind;
use crate::scope::Scope;
use crate::snapshot::Snapshot;

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

/// Durable message reads required by the invocation worker.
///
/// Provider-call lifecycle writes are intentionally not exposed here: response,
/// attempt state, usage settlement, and terminal transitions must use the
/// lease-fenced [`crate::invocation_lease::InvocationLeaseStore`] contract.
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
}

/// Persistence boundary for creating a new tenant-owned Draft object.
///
/// Implementations must persist only within the supplied authenticated Scope
/// and reject an Object whose tenant does not match that Scope.
pub trait ObjectCreationStore: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn create_object(
        &self,
        scope: &Scope,
        object: &crate::object::Object,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Result of atomically creating a Draft version and its immutable snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftVersionCreation {
    Created {
        version_id: crate::ids::ObjectVersionId,
        snapshot_id: crate::ids::SnapshotId,
    },
    IdenticalContent {
        version_id: crate::ids::ObjectVersionId,
        snapshot_id: crate::ids::SnapshotId,
    },
}

/// Persistence boundary for creating a Draft version together with its snapshot.
pub trait ObjectVersionCreationStore: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn create_draft_version_with_snapshot(
        &self,
        scope: &Scope,
        version: &crate::object_version::ObjectVersion,
        snapshot: &crate::snapshot::Snapshot,
    ) -> impl Future<Output = Result<DraftVersionCreation, Self::Error>> + Send;
}

/// A Draft snapshot selected for trusted publication validation.
///
/// The expected pointer is captured before validation and is rechecked by the
/// Store during publication. This makes replacement publication a compare-and-set
/// operation even though validation runs outside the database transaction.
#[derive(Debug)]
pub struct PublicationCandidate {
    snapshot: Snapshot,
    object_state: ObjectState,
    expected_current: Option<ObjectVersionId>,
}

impl PublicationCandidate {
    #[must_use]
    pub const fn new(
        snapshot: Snapshot,
        object_state: ObjectState,
        expected_current: Option<ObjectVersionId>,
    ) -> Self {
        Self {
            snapshot,
            object_state,
            expected_current,
        }
    }

    #[must_use]
    pub const fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    #[must_use]
    pub const fn object_state(&self) -> ObjectState {
        self.object_state
    }

    #[must_use]
    pub const fn expected_current(&self) -> Option<ObjectVersionId> {
        self.expected_current
    }
}

/// Persistence boundary for trusted Owner publication.
///
/// Candidate reads and validation happen before the final publish transaction.
/// The Store owns lifecycle transitions, persisted evidence verification, and
/// the expected-current-pointer CAS.
pub trait ObjectPublicationStore: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn load_publication_candidate(
        &self,
        scope: &Scope,
        object_id: ObjectId,
        version_id: ObjectVersionId,
    ) -> impl Future<Output = Result<Option<PublicationCandidate>, Self::Error>> + Send;

    fn begin_validation(
        &self,
        scope: &Scope,
        object_id: ObjectId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn reject_validation(
        &self,
        scope: &Scope,
        object_id: ObjectId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn publish_version(
        &self,
        scope: &Scope,
        object_id: ObjectId,
        version_id: ObjectVersionId,
        expected_current: Option<ObjectVersionId>,
        evidence: &ValidationEvidence,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
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

/// Server-prepared inputs for the atomic begin-invocation persistence operation.
/// The Store resolves the selected Caller and current published Snapshot inside
/// one tenant-scoped transaction; transport input never supplies a Snapshot ID.
#[derive(Clone, Debug)]
pub struct InvocationStartCommand {
    pub object_id: ObjectId,
    pub requested_caller_id: CallerId,
    pub invocation_pool: InvocationPool,
    pub idempotency_key: String,
    pub payload_hash: Vec<u8>,
    pub input_message: String,
    pub message_expires_at: std::time::SystemTime,
    pub idempotency_expires_at: std::time::SystemTime,
    pub trace_id: TraceId,
}

/// The durable result of starting an invocation. Replays return the original ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvocationStartOutcome {
    Created { invocation_id: InvocationId },
    Replay { invocation_id: InvocationId },
}

/// Persistence boundary for the atomic begin-invocation transaction.
pub trait InvocationStartStore: Send + Sync {
    type Error: Error + Send + Sync + 'static;
    fn begin_invocation(
        &self,
        scope: &Scope,
        command: InvocationStartCommand,
    ) -> impl Future<Output = Result<InvocationStartOutcome, Self::Error>> + Send;
}

/// Result of an idempotent invocation cancellation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvocationCancellationOutcome {
    Cancelled { newly_cancelled: bool },
    AlreadyTerminal(crate::invocation::InvocationStatus),
    NotFound,
}

/// Persistence boundary for durable invocation cancellation.
pub trait InvocationCancellationStore: Send + Sync {
    type Error: Error + Send + Sync + 'static;
    fn cancel_invocation(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
    ) -> impl Future<Output = Result<InvocationCancellationOutcome, Self::Error>> + Send;
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

/// Broker-neutral message handed to an outbound publisher.
///
/// The stable delivery ID is the idempotency identity across retries. The
/// subject and JSON payload are selected from the committed PostgreSQL Outbox
/// projection; tenant identity is not promoted into transport authority.
#[derive(Clone, Debug, PartialEq)]
pub struct OutboundMessage {
    delivery_id: String,
    subject: String,
    payload: serde_json::Value,
}

impl OutboundMessage {
    #[must_use]
    pub fn new(
        delivery_id: impl Into<String>,
        subject: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            delivery_id: delivery_id.into(),
            subject: subject.into(),
            payload,
        }
    }

    #[must_use]
    pub fn delivery_id(&self) -> &str {
        &self.delivery_id
    }

    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    #[must_use]
    pub fn payload(&self) -> &serde_json::Value {
        &self.payload
    }
}

/// Transport publication receipt, normalized away from any broker SDK type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationReceipt {
    stream: String,
    sequence: u64,
    duplicate: bool,
}

impl PublicationReceipt {
    #[must_use]
    pub fn new(stream: impl Into<String>, sequence: u64, duplicate: bool) -> Self {
        Self {
            stream: stream.into(),
            sequence,
            duplicate,
        }
    }

    #[must_use]
    pub fn stream(&self) -> &str {
        &self.stream
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn duplicate(&self) -> bool {
        self.duplicate
    }
}

/// A message received from the durable transport. Its delivery identity is
/// supplied by the trusted publisher header, not inferred from payload fields.
#[derive(Clone, Debug, PartialEq)]
pub struct InboundMessage {
    delivery_id: String,
    subject: String,
    payload: serde_json::Value,
}

impl InboundMessage {
    #[must_use]
    pub fn new(
        delivery_id: impl Into<String>,
        subject: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            delivery_id: delivery_id.into(),
            subject: subject.into(),
            payload,
        }
    }

    #[must_use]
    pub fn delivery_id(&self) -> &str {
        &self.delivery_id
    }

    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    #[must_use]
    pub fn payload(&self) -> &serde_json::Value {
        &self.payload
    }
}

/// Whether a handler failure is expected to succeed if the same message is retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageFailureKind {
    Retryable,
    Permanent,
}

/// A classified application failure used by the consumer retry/DLQ policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageHandlerError {
    kind: MessageFailureKind,
    code: String,
    message: String,
}

impl MessageHandlerError {
    #[must_use]
    pub fn retryable(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { kind: MessageFailureKind::Retryable, code: code.into(), message: message.into() }
    }

    #[must_use]
    pub fn permanent(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { kind: MessageFailureKind::Permanent, code: code.into(), message: message.into() }
    }

    #[must_use]
    pub const fn kind(&self) -> MessageFailureKind { self.kind }

    #[must_use]
    pub fn code(&self) -> &str { &self.code }

    #[must_use]
    pub fn message(&self) -> &str { &self.message }
}

impl std::fmt::Display for MessageHandlerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for MessageHandlerError {}

/// A handler may return Ok only after its durable PostgreSQL work has committed.
pub trait InboundMessageHandler: Send + Sync {
    fn handle(
        &self,
        message: InboundMessage,
    ) -> Pin<Box<dyn Future<Output = Result<(), MessageHandlerError>> + Send + '_>>;
}

/// Counts the result of one bounded pull-consumer batch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageConsumerOutcome {
    pub received: usize,
    pub acknowledged: usize,
    pub handler_failed: usize,
    pub retry_scheduled: usize,
    pub dead_lettered: usize,
    pub dead_letter_failed: usize,
}

/// Broker-neutral pull-consumer boundary. Implementations ACK only after
/// the handler succeeds; handlers must return success after PostgreSQL commit.
pub trait MessageConsumer: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn process_batch<'a>(
        &'a self,
        handler: &'a dyn InboundMessageHandler,
        batch_size: u16,
        expires_after: Duration,
    ) -> impl Future<Output = Result<MessageConsumerOutcome, Self::Error>> + Send + 'a;
}

/// Broker-neutral outbound publication port.
///
/// Returning a receipt means the transport has acknowledged durable acceptance;
/// it does not mean that a business consumer completed the message.
pub trait MessagePublisher: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn publish(
        &self,
        message: OutboundMessage,
    ) -> impl Future<Output = Result<PublicationReceipt, Self::Error>> + Send;
}

/// Persistence port used by the Outbox Relay.
///
/// Claim and receipt writes are short PostgreSQL transactions. Implementations
/// must not keep a database transaction open while the publisher performs I/O.
pub trait OutboxDeliveryStore: Send + Sync {
    type Delivery: Send + Sync;
    type Error: Error + Send + Sync + 'static;

    fn claim_batch(
        &self,
        owner: &str,
        topic: &str,
        batch_size: u16,
        lease_seconds: u32,
    ) -> impl Future<Output = Result<Vec<Self::Delivery>, Self::Error>> + Send;

    fn outbound_message(&self, delivery: &Self::Delivery) -> OutboundMessage;

    fn record_publication(
        &self,
        delivery: &Self::Delivery,
        owner: &str,
        receipt: PublicationReceipt,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;

    fn release(
        &self,
        delivery: &Self::Delivery,
        owner: &str,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
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
