//! Minimal durable invocation worker.
//!
//! This worker claims a PostgreSQL lease, heartbeats during provider I/O, and
//! fences every lifecycle write by owner, expiry, version, and fencing token.

use std::error::Error;
use std::fmt;
use std::time::Duration;

use qembed_domain::chat::{
    ChatContractError, ChatMessage, ChatRequest, ChatRole, ProviderRequestContext,
};
use qembed_domain::ids::InvocationId;
use qembed_domain::invocation::{Invocation, InvocationError, InvocationStatus};
use qembed_domain::invocation_lease::InvocationLeaseStore;
use qembed_domain::ports::ChatProvider;
use qembed_domain::provider_call_attempt::ProviderCallAttemptState;
use qembed_domain::scope::Scope;

/// Application-owned model route and output budget for the mock-first path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvocationWorkerConfig {
    model_key: String,
    max_output_tokens: u32,
    worker_id: String,
    lease_seconds: u32,
}

impl InvocationWorkerConfig {
    pub fn new(
        model_key: impl Into<String>,
        max_output_tokens: u32,
    ) -> Result<Self, ChatContractError> {
        let model_key = model_key.into();
        ChatRequest::new(
            model_key.clone(),
            vec![ChatMessage::new(ChatRole::User, "validation")?],
            max_output_tokens,
        )?;
        Ok(Self {
            model_key,
            max_output_tokens,
            worker_id: format!("worker-{}", InvocationId::new()),
            lease_seconds: 60,
        })
    }

    /// Overrides the generated worker identity and lease duration.
    ///
    /// The owner must be unique to this live worker instance. Lease durations
    /// are validated by the PostgreSQL adapter; 3..=3600 seconds are supported.
    #[must_use]
    pub fn with_lease(mut self, worker_id: impl Into<String>, lease_seconds: u32) -> Self {
        self.worker_id = worker_id.into();
        self.lease_seconds = lease_seconds;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvocationWorkerOutcome {
    Completed {
        invocation_id: InvocationId,
        content: String,
    },
    Failed {
        invocation_id: InvocationId,
        error_code: &'static str,
    },
    OutcomeUnknown {
        invocation_id: InvocationId,
    },
    AlreadyTerminal {
        invocation_id: InvocationId,
        status: InvocationStatus,
    },
}

#[derive(Debug)]
pub enum InvocationWorkerError<E> {
    Store(E),
    NotFound,
    MissingUserInput,
    MissingPersistedOutput,
    ProviderRouteNotConfigured,
    InvalidRequest(ChatContractError),
    InvalidTransition(InvocationError),
    Conflict,
    UnsupportedState(InvocationStatus),
}

impl<E: fmt::Display> fmt::Display for InvocationWorkerError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "invocation worker store failed: {error}"),
            Self::NotFound => formatter.write_str("invocation was not found in this tenant"),
            Self::MissingUserInput => formatter.write_str("invocation has no durable user input"),
            Self::MissingPersistedOutput => {
                formatter.write_str("Verifying invocation has no durable assistant output")
            }
            Self::ProviderRouteNotConfigured => {
                formatter.write_str("configured chat provider route is unavailable")
            }
            Self::InvalidRequest(error) => {
                write!(formatter, "invalid worker chat request: {error}")
            }
            Self::InvalidTransition(error) => {
                write!(formatter, "invalid invocation transition: {error:?}")
            }
            Self::Conflict => {
                formatter.write_str("invocation changed concurrently; reload before retrying")
            }
            Self::UnsupportedState(status) => write!(
                formatter,
                "worker does not process invocation state {status:?}"
            ),
        }
    }
}

impl<E: Error + 'static> Error for InvocationWorkerError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::InvalidRequest(error) => Some(error),
            _ => None,
        }
    }
}

/// Coordinates an invocation using domain ports; it owns no SQL or SDK details.
pub struct InvocationWorker<S, P> {
    store: S,
    provider: P,
    config: InvocationWorkerConfig,
}

impl<S, P> InvocationWorker<S, P>
where
    S: InvocationLeaseStore,
    P: ChatProvider,
{
    #[must_use]
    pub fn new(store: S, provider: P, config: InvocationWorkerConfig) -> Self {
        Self {
            store,
            provider,
            config,
        }
    }

    /// Advances one invocation until terminal completion or a persistence conflict.
    ///
    /// Provider I/O always happens outside SQL transactions. The provider result
    /// and Processing → Verifying transition commit together. A restart at
    /// Verifying loads the saved assistant message and finishes without a new call.
    pub async fn process(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
    ) -> Result<InvocationWorkerOutcome, InvocationWorkerError<S::Error>> {
        let claimed = self
            .store
            .claim_invocation(
                scope,
                invocation_id,
                &self.config.worker_id,
                self.config.lease_seconds,
            )
            .await
            .map_err(InvocationWorkerError::Store)?;
        if claimed.is_none() {
            let existing = self
                .store
                .load_invocation(scope, invocation_id)
                .await
                .map_err(InvocationWorkerError::Store)?
                .ok_or(InvocationWorkerError::NotFound)?;
            return match existing.status() {
                InvocationStatus::Completed | InvocationStatus::CompletedDegraded => {
                    let content = self
                        .store
                        .load_assistant_output(scope, invocation_id)
                        .await
                        .map_err(InvocationWorkerError::Store)?
                        .ok_or(InvocationWorkerError::MissingPersistedOutput)?;
                    Ok(InvocationWorkerOutcome::Completed {
                        invocation_id,
                        content,
                    })
                }
                status if status.is_terminal() => Ok(InvocationWorkerOutcome::AlreadyTerminal {
                    invocation_id,
                    status,
                }),
                _ => Err(InvocationWorkerError::Conflict),
            };
        }

        for _ in 0..8 {
            let invocation = self
                .store
                .load_invocation(scope, invocation_id)
                .await
                .map_err(InvocationWorkerError::Store)?
                .ok_or(InvocationWorkerError::NotFound)?;
            let live = self
                .store
                .heartbeat_invocation(
                    scope,
                    invocation_id,
                    &self.config.worker_id,
                    invocation.fencing_token(),
                    self.config.lease_seconds,
                )
                .await
                .map_err(InvocationWorkerError::Store)?;
            if !live {
                return Err(InvocationWorkerError::Conflict);
            }

            match invocation.status() {
                InvocationStatus::Received => {
                    self.transition(scope, &invocation, InvocationStatus::ContextLoading)
                        .await?;
                }
                InvocationStatus::ContextLoading => {
                    self.transition(scope, &invocation, InvocationStatus::Processing)
                        .await?;
                }
                InvocationStatus::Processing => {
                    let attempt = self
                        .store
                        .load_provider_call_attempt(scope, invocation_id)
                        .await
                        .map_err(InvocationWorkerError::Store)?
                        .ok_or(InvocationWorkerError::Conflict)?;
                    match attempt.state() {
                        ProviderCallAttemptState::ResponseReceived => {
                            return Err(InvocationWorkerError::Conflict);
                        }
                        ProviderCallAttemptState::MayHaveBeenSent
                        | ProviderCallAttemptState::OutcomeUnknown => {
                            if attempt.state() == ProviderCallAttemptState::MayHaveBeenSent {
                                let marked = self
                                    .store
                                    .mark_provider_outcome_unknown_owned(
                                        scope,
                                        invocation_id,
                                        &self.config.worker_id,
                                        invocation.version(),
                                        invocation.fencing_token(),
                                    )
                                    .await
                                    .map_err(InvocationWorkerError::Store)?;
                                if !marked {
                                    return Err(InvocationWorkerError::Conflict);
                                }
                            }
                            return Ok(InvocationWorkerOutcome::OutcomeUnknown { invocation_id });
                        }
                        ProviderCallAttemptState::ConfirmedNotAccepted
                        | ProviderCallAttemptState::CancelledBeforeSend => {
                            return Err(InvocationWorkerError::Conflict);
                        }
                        ProviderCallAttemptState::NotStarted => {}
                    }
                    let input = self
                        .store
                        .load_user_input(scope, invocation_id)
                        .await
                        .map_err(InvocationWorkerError::Store)?
                        .ok_or(InvocationWorkerError::MissingUserInput)?;
                    let message = ChatMessage::new(ChatRole::User, input)
                        .map_err(InvocationWorkerError::InvalidRequest)?;
                    let request = ChatRequest::new(
                        self.config.model_key.clone(),
                        vec![message],
                        self.config.max_output_tokens,
                    )
                    .map_err(InvocationWorkerError::InvalidRequest)?;

                    if self.provider.capabilities(&self.config.model_key).is_none() {
                        return Err(InvocationWorkerError::ProviderRouteNotConfigured);
                    }

                    let may_send = self
                        .store
                        .mark_provider_may_have_been_sent_owned(
                            scope,
                            invocation_id,
                            &self.config.worker_id,
                            invocation.version(),
                            invocation.fencing_token(),
                        )
                        .await
                        .map_err(InvocationWorkerError::Store)?;
                    if !may_send {
                        return Err(InvocationWorkerError::Conflict);
                    }

                    let provider_context = ProviderRequestContext::for_invocation(invocation_id);
                    let provider_future = self
                        .provider
                        .complete_with_context(request, provider_context);
                    tokio::pin!(provider_future);
                    let heartbeat_period =
                        Duration::from_secs(u64::from((self.config.lease_seconds / 3).max(1)));
                    let mut heartbeat = tokio::time::interval(heartbeat_period);
                    heartbeat.tick().await;
                    let provider_result = loop {
                        tokio::select! {
                            result = &mut provider_future => break result,
                            _ = heartbeat.tick() => {
                                let renewed = self.store.heartbeat_invocation(
                                    scope,
                                    invocation_id,
                                    &self.config.worker_id,
                                    invocation.fencing_token(),
                                    self.config.lease_seconds,
                                ).await.map_err(InvocationWorkerError::Store)?;
                                if !renewed {
                                    return Err(InvocationWorkerError::Conflict);
                                }
                            }
                        }
                    };

                    match provider_result {
                        Ok(response) => {
                            let mut next = invocation.clone();
                            next.transition_to(
                                InvocationStatus::Verifying,
                                invocation.fencing_token(),
                            )
                            .map_err(InvocationWorkerError::InvalidTransition)?;
                            let persisted = self
                                .store
                                .persist_provider_response_owned(
                                    scope,
                                    &self.config.worker_id,
                                    invocation.version(),
                                    invocation.fencing_token(),
                                    next,
                                    response,
                                )
                                .await
                                .map_err(InvocationWorkerError::Store)?;
                            if !persisted {
                                return Err(InvocationWorkerError::Conflict);
                            }
                        }
                        Err(_error) => {
                            let persisted = self
                                .store
                                .mark_provider_outcome_unknown_owned(
                                    scope,
                                    invocation_id,
                                    &self.config.worker_id,
                                    invocation.version(),
                                    invocation.fencing_token(),
                                )
                                .await
                                .map_err(InvocationWorkerError::Store)?;
                            if !persisted {
                                return Err(InvocationWorkerError::Conflict);
                            }
                            return Ok(InvocationWorkerOutcome::OutcomeUnknown { invocation_id });
                        }
                    }
                }
                InvocationStatus::Verifying => {
                    let content = self
                        .store
                        .load_assistant_output(scope, invocation_id)
                        .await
                        .map_err(InvocationWorkerError::Store)?
                        .ok_or(InvocationWorkerError::MissingPersistedOutput)?;
                    let mut next = invocation.clone();
                    next.transition_to(InvocationStatus::Completed, invocation.fencing_token())
                        .map_err(InvocationWorkerError::InvalidTransition)?;
                    let completed = self
                        .store
                        .complete_invocation_owned(
                            scope,
                            &self.config.worker_id,
                            invocation.version(),
                            invocation.fencing_token(),
                            next,
                        )
                        .await
                        .map_err(InvocationWorkerError::Store)?;
                    if !completed {
                        return Err(InvocationWorkerError::Conflict);
                    }
                    return Ok(InvocationWorkerOutcome::Completed {
                        invocation_id,
                        content,
                    });
                }
                InvocationStatus::Completed | InvocationStatus::CompletedDegraded => {
                    let content = self
                        .store
                        .load_assistant_output(scope, invocation_id)
                        .await
                        .map_err(InvocationWorkerError::Store)?
                        .ok_or(InvocationWorkerError::MissingPersistedOutput)?;
                    return Ok(InvocationWorkerOutcome::Completed {
                        invocation_id,
                        content,
                    });
                }
                InvocationStatus::Failed | InvocationStatus::Cancelled => {
                    return Ok(InvocationWorkerOutcome::AlreadyTerminal {
                        invocation_id,
                        status: invocation.status(),
                    });
                }
                status => return Err(InvocationWorkerError::UnsupportedState(status)),
            }
        }
        Err(InvocationWorkerError::Conflict)
    }

    async fn transition(
        &self,
        scope: &Scope,
        current: &Invocation,
        next_status: InvocationStatus,
    ) -> Result<(), InvocationWorkerError<S::Error>> {
        let mut next = current.clone();
        next.transition_to(next_status, current.fencing_token())
            .map_err(InvocationWorkerError::InvalidTransition)?;
        let changed = self
            .store
            .compare_and_set_owned(
                scope,
                &self.config.worker_id,
                current.version(),
                current.fencing_token(),
                next,
            )
            .await
            .map_err(InvocationWorkerError::Store)?;
        if changed {
            Ok(())
        } else {
            Err(InvocationWorkerError::Conflict)
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use qembed_domain::chat::{
        ChatCapabilities, ChatFinishReason, ChatProviderError, ChatResponse, ProviderRequestContext,
    };
    use qembed_domain::ids::{CallerId, ObjectId, SnapshotId, TenantId};
    use qembed_domain::invocation::{FencingToken, Invocation, InvocationCheckpoint};
    use qembed_domain::invocation_lease::InvocationLeaseStore;
    use qembed_domain::ports::{ExecutionStore, InvocationWorkStore};
    use qembed_domain::provider_call_attempt::{ProviderCallAttempt, ProviderCallAttemptState};
    use qembed_domain::test_support::scope_for_tenant;

    use super::*;

    struct State {
        invocation: Invocation,
        input: String,
        output: Option<String>,
        error_code: Option<&'static str>,
        lease_owner: Option<String>,
        attempt: ProviderCallAttemptState,
        usage: Option<qembed_domain::chat::TokenUsage>,
    }

    #[derive(Clone)]
    struct FakeStore(Arc<Mutex<State>>);

    impl ExecutionStore for FakeStore {
        type Error = Infallible;

        async fn load_invocation(
            &self,
            scope: &Scope,
            invocation_id: InvocationId,
        ) -> Result<Option<Invocation>, Self::Error> {
            let state = self.0.lock().expect("fake store lock");
            Ok((state.invocation.tenant_id() == scope.tenant_id()
                && state.invocation.id() == invocation_id)
                .then(|| state.invocation.clone()))
        }

        async fn compare_and_set_invocation(
            &self,
            scope: &Scope,
            expected_version: u64,
            expected_fencing_token: FencingToken,
            next: Invocation,
        ) -> Result<bool, Self::Error> {
            let mut state = self.0.lock().expect("fake store lock");
            if state.invocation.tenant_id() != scope.tenant_id()
                || state.invocation.version() != expected_version
                || state.invocation.fencing_token() != expected_fencing_token
            {
                return Ok(false);
            }
            state.invocation = next;
            Ok(true)
        }
    }

    impl InvocationWorkStore for FakeStore {
        async fn load_user_input(
            &self,
            scope: &Scope,
            invocation_id: InvocationId,
        ) -> Result<Option<String>, Self::Error> {
            let state = self.0.lock().expect("fake store lock");
            Ok((state.invocation.tenant_id() == scope.tenant_id()
                && state.invocation.id() == invocation_id)
                .then(|| state.input.clone()))
        }

        async fn load_assistant_output(
            &self,
            scope: &Scope,
            invocation_id: InvocationId,
        ) -> Result<Option<String>, Self::Error> {
            let state = self.0.lock().expect("fake store lock");
            Ok(
                if state.invocation.tenant_id() == scope.tenant_id()
                    && state.invocation.id() == invocation_id
                {
                    state.output.clone()
                } else {
                    None
                },
            )
        }


    }

    impl InvocationLeaseStore for FakeStore {
        async fn claim_invocation(
            &self,
            scope: &Scope,
            invocation_id: InvocationId,
            owner: &str,
            _: u32,
        ) -> Result<Option<Invocation>, Self::Error> {
            let mut state = self.0.lock().expect("fake store lock");
            if state.invocation.tenant_id() != scope.tenant_id()
                || state.invocation.id() != invocation_id
                || state.invocation.status().is_terminal()
                || state.lease_owner.is_some()
            {
                return Ok(None);
            }
            state.lease_owner = Some(owner.to_owned());
            Ok(Some(state.invocation.clone()))
        }

        async fn heartbeat_invocation(
            &self,
            scope: &Scope,
            invocation_id: InvocationId,
            owner: &str,
            token: FencingToken,
            _: u32,
        ) -> Result<bool, Self::Error> {
            let state = self.0.lock().expect("fake store lock");
            Ok(state.invocation.tenant_id() == scope.tenant_id()
                && state.invocation.id() == invocation_id
                && state.invocation.fencing_token() == token
                && state.lease_owner.as_deref() == Some(owner))
        }

        async fn compare_and_set_owned(
            &self,
            scope: &Scope,
            owner: &str,
            version: u64,
            token: FencingToken,
            next: Invocation,
        ) -> Result<bool, Self::Error> {
            if self
                .0
                .lock()
                .expect("fake store lock")
                .lease_owner
                .as_deref()
                != Some(owner)
            {
                return Ok(false);
            }
            self.compare_and_set_invocation(scope, version, token, next)
                .await
        }

        async fn persist_assistant_output_owned(
            &self,
            scope: &Scope,
            owner: &str,
            version: u64,
            token: FencingToken,
            next: Invocation,
            content: String,
        ) -> Result<bool, Self::Error> {
            let response = ChatResponse::new(content, ChatFinishReason::Stop, None)
                .expect("legacy fake response should satisfy the response contract");
            self.persist_provider_response_owned(scope, owner, version, token, next, response)
                .await
        }

        async fn load_provider_call_attempt(
            &self,
            scope: &Scope,
            invocation_id: InvocationId,
        ) -> Result<Option<ProviderCallAttempt>, Self::Error> {
            let state = self.0.lock().expect("fake store lock");
            Ok((state.invocation.tenant_id() == scope.tenant_id()
                && state.invocation.id() == invocation_id)
                .then(|| ProviderCallAttempt::from_persisted(state.attempt, state.usage)))
        }

        async fn mark_provider_may_have_been_sent_owned(
            &self,
            scope: &Scope,
            invocation_id: InvocationId,
            owner: &str,
            version: u64,
            token: FencingToken,
        ) -> Result<bool, Self::Error> {
            let mut state = self.0.lock().expect("fake store lock");
            if state.invocation.tenant_id() != scope.tenant_id()
                || state.invocation.id() != invocation_id
                || state.invocation.version() != version
                || state.invocation.fencing_token() != token
                || state.lease_owner.as_deref() != Some(owner)
                || state.attempt != ProviderCallAttemptState::NotStarted
            {
                return Ok(false);
            }
            state.attempt = ProviderCallAttemptState::MayHaveBeenSent;
            Ok(true)
        }

        async fn mark_provider_outcome_unknown_owned(
            &self,
            scope: &Scope,
            invocation_id: InvocationId,
            owner: &str,
            version: u64,
            token: FencingToken,
        ) -> Result<bool, Self::Error> {
            let mut state = self.0.lock().expect("fake store lock");
            if state.invocation.tenant_id() != scope.tenant_id()
                || state.invocation.id() != invocation_id
                || state.invocation.version() != version
                || state.invocation.fencing_token() != token
                || state.lease_owner.as_deref() != Some(owner)
                || state.attempt != ProviderCallAttemptState::MayHaveBeenSent
            {
                return Ok(false);
            }
            state.attempt = ProviderCallAttemptState::OutcomeUnknown;
            Ok(true)
        }

        async fn persist_provider_response_owned(
            &self,
            scope: &Scope,
            owner: &str,
            version: u64,
            token: FencingToken,
            next: Invocation,
            response: qembed_domain::chat::ChatResponse,
        ) -> Result<bool, Self::Error> {
            let mut state = self.0.lock().expect("fake store lock");
            if state.invocation.tenant_id() != scope.tenant_id()
                || state.invocation.version() != version
                || state.invocation.fencing_token() != token
                || state.lease_owner.as_deref() != Some(owner)
                || state.attempt != ProviderCallAttemptState::MayHaveBeenSent
                || state.invocation.status() != InvocationStatus::Processing
            {
                return Ok(false);
            }
            state.output = Some(response.content().to_owned());
            state.usage = response.usage();
            state.attempt = ProviderCallAttemptState::ResponseReceived;
            state.invocation = next;
            Ok(true)
        }

        async fn complete_invocation_owned(
            &self,
            scope: &Scope,
            owner: &str,
            version: u64,
            token: FencingToken,
            next: Invocation,
        ) -> Result<bool, Self::Error> {
            if self
                .0
                .lock()
                .expect("fake store lock")
                .lease_owner
                .as_deref()
                != Some(owner)
            {
                return Ok(false);
            }
            self.complete_invocation(scope, version, token, next).await
        }

        async fn fail_invocation_owned(
            &self,
            scope: &Scope,
            owner: &str,
            version: u64,
            token: FencingToken,
            next: Invocation,
            error_code: &'static str,
        ) -> Result<bool, Self::Error> {
            if self
                .0
                .lock()
                .expect("fake store lock")
                .lease_owner
                .as_deref()
                != Some(owner)
            {
                return Ok(false);
            }
            self.fail_invocation(scope, version, token, next, error_code)
                .await
        }
    }

    struct FakeProvider {
        response: Result<ChatResponse, ChatProviderError>,
        calls: Arc<AtomicUsize>,
        contexts: Arc<Mutex<Vec<String>>>,
    }

    impl ChatProvider for FakeProvider {
        fn capabilities(&self, _: &str) -> Option<ChatCapabilities> {
            Some(ChatCapabilities::new(std::iter::empty()))
        }

        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse, ChatProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.response.clone()
        }

        async fn complete_with_context(
            &self,
            request: ChatRequest,
            context: ProviderRequestContext,
        ) -> Result<ChatResponse, ChatProviderError> {
            self.contexts
                .lock()
                .expect("provider context lock")
                .push(context.idempotency_key().to_owned());
            self.complete(request).await
        }
    }

    fn fixture(
        status: InvocationStatus,
        version: u64,
        output: Option<&str>,
    ) -> (Scope, InvocationId, FakeStore) {
        let tenant_id = TenantId::new();
        let scope = scope_for_tenant(tenant_id);
        let invocation_id = InvocationId::new();
        let invocation = Invocation::from_persisted(
            invocation_id,
            tenant_id,
            ObjectId::new(),
            CallerId::new(),
            SnapshotId::new(),
            InvocationCheckpoint::new(status, version, FencingToken::INITIAL),
        );
        let store = FakeStore(Arc::new(Mutex::new(State {
            invocation,
            input: "hello".to_owned(),
            output: output.map(str::to_owned),
            error_code: None,
            lease_owner: None,
            attempt: if status == InvocationStatus::Verifying && output.is_some() {
                ProviderCallAttemptState::ResponseReceived
            } else {
                ProviderCallAttemptState::NotStarted
            },
            usage: None,
        })));
        (scope, invocation_id, store)
    }

    type WorkerHarness = (
        InvocationWorker<FakeStore, FakeProvider>,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<String>>>,
    );

    fn worker(
        store: FakeStore,
        response: Result<ChatResponse, ChatProviderError>,
    ) -> WorkerHarness {
        let calls = Arc::new(AtomicUsize::new(0));
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let provider = FakeProvider {
            response,
            calls: Arc::clone(&calls),
            contexts: Arc::clone(&contexts),
        };
        let config = InvocationWorkerConfig::new("test-chat", 128).expect("valid worker config");
        (
            InvocationWorker::new(store, provider, config),
            calls,
            contexts,
        )
    }

    #[tokio::test]
    async fn worker_processes_and_finishes_with_durable_result() {
        let (scope, id, store) = fixture(InvocationStatus::Received, 0, None);
        let response = ChatResponse::new("hello back", ChatFinishReason::Stop, None)
            .expect("valid provider response");
        let (worker, calls, contexts) = worker(store.clone(), Ok(response));

        let outcome = worker.process(&scope, id).await.expect("worker completes");
        assert_eq!(
            outcome,
            InvocationWorkerOutcome::Completed {
                invocation_id: id,
                content: "hello back".to_owned(),
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            contexts.lock().expect("provider context lock").as_slice(),
            &[ProviderRequestContext::for_invocation(id)
                .idempotency_key()
                .to_owned()]
        );
        let state = store.0.lock().expect("fake store lock");
        assert_eq!(
            state.invocation.status(),
            InvocationStatus::CompletedDegraded
        );
        assert_eq!(state.output.as_deref(), Some("hello back"));
    }

    #[tokio::test]
    async fn restart_at_verifying_finishes_from_persisted_output_without_provider_call() {
        let (scope, id, store) = fixture(InvocationStatus::Verifying, 3, Some("saved answer"));
        let response = ChatResponse::new("must not be used", ChatFinishReason::Stop, None)
            .expect("valid provider response");
        let (worker, calls, _contexts) = worker(store.clone(), Ok(response));

        let outcome = worker.process(&scope, id).await.expect("worker resumes");
        assert_eq!(
            outcome,
            InvocationWorkerOutcome::Completed {
                invocation_id: id,
                content: "saved answer".to_owned(),
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            store.0.lock().expect("fake store lock").invocation.status(),
            InvocationStatus::CompletedDegraded
        );
    }

    #[tokio::test]
    async fn ambiguous_provider_failure_keeps_invocation_and_reservation_unresolved() {
        let (scope, id, store) = fixture(InvocationStatus::Processing, 2, None);
        let (worker, calls, _contexts) = worker(store.clone(), Err(ChatProviderError::Timeout));

        let outcome = worker
            .process(&scope, id)
            .await
            .expect("ambiguous outcome is recorded");
        assert_eq!(
            outcome,
            InvocationWorkerOutcome::OutcomeUnknown { invocation_id: id }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let state = store.0.lock().expect("fake store lock");
        assert_eq!(state.invocation.status(), InvocationStatus::Processing);
        assert_eq!(state.attempt, ProviderCallAttemptState::OutcomeUnknown);
        assert_eq!(state.error_code, None);
    }

    #[tokio::test]
    async fn crash_after_send_marker_is_reconciled_without_resending() {
        let (scope, id, store) = fixture(InvocationStatus::Processing, 2, None);
        {
            let mut state = store.0.lock().expect("fake store lock");
            state.attempt = ProviderCallAttemptState::MayHaveBeenSent;
        }
        let response = ChatResponse::new("must not be used", ChatFinishReason::Stop, None)
            .expect("valid provider response");
        let (worker, calls, _) = worker(store.clone(), Ok(response));
        let outcome = worker
            .process(&scope, id)
            .await
            .expect("recovery remains ambiguous");
        assert_eq!(
            outcome,
            InvocationWorkerOutcome::OutcomeUnknown { invocation_id: id }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            store.0.lock().expect("fake store lock").attempt,
            ProviderCallAttemptState::OutcomeUnknown
        );
    }

    #[tokio::test]
    async fn restart_after_ambiguous_attempt_does_not_call_provider_again() {
        let (scope, id, store) = fixture(InvocationStatus::Processing, 2, None);
        {
            let mut state = store.0.lock().expect("fake store lock");
            state.attempt = ProviderCallAttemptState::OutcomeUnknown;
        }
        let response = ChatResponse::new("must not be used", ChatFinishReason::Stop, None)
            .expect("valid provider response");
        let (worker, calls, _) = worker(store.clone(), Ok(response));
        let outcome = worker
            .process(&scope, id)
            .await
            .expect("worker does not resend");
        assert_eq!(
            outcome,
            InvocationWorkerOutcome::OutcomeUnknown { invocation_id: id }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
