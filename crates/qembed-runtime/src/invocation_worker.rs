//! Minimal durable invocation worker.
//!
//! This worker is lease-free in RUST-037. It persists each state boundary and
//! the provider response so a restarted process resumes from Verifying without
//! another provider call. Lease acquisition/reclaim belongs to RUST-038.

use std::error::Error;
use std::fmt;

use qembed_domain::chat::{ChatContractError, ChatMessage, ChatProviderError, ChatRequest, ChatRole};
use qembed_domain::ids::InvocationId;
use qembed_domain::invocation::{Invocation, InvocationError, InvocationStatus};
use qembed_domain::ports::{ChatProvider, InvocationWorkStore};
use qembed_domain::scope::Scope;

/// Application-owned model route and output budget for the mock-first path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvocationWorkerConfig {
    model_key: String,
    max_output_tokens: u32,
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
        })
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
            Self::MissingPersistedOutput => formatter.write_str("Verifying invocation has no durable assistant output"),
            Self::ProviderRouteNotConfigured => formatter.write_str("configured chat provider route is unavailable"),
            Self::InvalidRequest(error) => write!(formatter, "invalid worker chat request: {error}"),
            Self::InvalidTransition(error) => write!(formatter, "invalid invocation transition: {error:?}"),
            Self::Conflict => formatter.write_str("invocation changed concurrently; reload before retrying"),
            Self::UnsupportedState(status) => write!(formatter, "worker does not process invocation state {status:?}"),
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
    S: InvocationWorkStore,
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
        for _ in 0..8 {
            let invocation = self
                .store
                .load_invocation(scope, invocation_id)
                .await
                .map_err(InvocationWorkerError::Store)?
                .ok_or(InvocationWorkerError::NotFound)?;

            match invocation.status() {
                InvocationStatus::Received => {
                    self.transition(scope, &invocation, InvocationStatus::ContextLoading).await?;
                }
                InvocationStatus::ContextLoading => {
                    self.transition(scope, &invocation, InvocationStatus::Processing).await?;
                }
                InvocationStatus::Processing => {
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

                    match self.provider.complete(request).await {
                        Ok(response) => {
                            let mut next = invocation.clone();
                            next.transition_to(
                                InvocationStatus::Verifying,
                                invocation.fencing_token(),
                            )
                            .map_err(InvocationWorkerError::InvalidTransition)?;
                            let persisted = self
                                .store
                                .persist_assistant_output(
                                    scope,
                                    invocation.version(),
                                    invocation.fencing_token(),
                                    next,
                                    response.content().to_owned(),
                                )
                                .await
                                .map_err(InvocationWorkerError::Store)?;
                            if !persisted {
                                return Err(InvocationWorkerError::Conflict);
                            }
                        }
                        Err(error) => {
                            let code = provider_error_code(error);
                            let mut next = invocation.clone();
                            next.transition_to(
                                InvocationStatus::Failed,
                                invocation.fencing_token(),
                            )
                            .map_err(InvocationWorkerError::InvalidTransition)?;
                            let persisted = self
                                .store
                                .fail_invocation(
                                    scope,
                                    invocation.version(),
                                    invocation.fencing_token(),
                                    next,
                                    code,
                                )
                                .await
                                .map_err(InvocationWorkerError::Store)?;
                            if !persisted {
                                return Err(InvocationWorkerError::Conflict);
                            }
                            return Ok(InvocationWorkerOutcome::Failed {
                                invocation_id,
                                error_code: code,
                            });
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
                    next.transition_to(
                        InvocationStatus::Completed,
                        invocation.fencing_token(),
                    )
                    .map_err(InvocationWorkerError::InvalidTransition)?;
                    let completed = self
                        .store
                        .complete_invocation(
                            scope,
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
            .compare_and_set_invocation(
                scope,
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

const fn provider_error_code(error: ChatProviderError) -> &'static str {
    match error {
        ChatProviderError::Timeout => "provider_timeout",
        ChatProviderError::RateLimited { .. } => "provider_rate_limited",
        ChatProviderError::Unavailable => "provider_unavailable",
        ChatProviderError::MalformedOutput => "provider_malformed_output",
        ChatProviderError::Other => "provider_other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_configuration_uses_domain_request_validation() {
        assert!(InvocationWorkerConfig::new("test-chat", 128).is_ok());
        assert!(InvocationWorkerConfig::new("", 128).is_err());
        assert!(InvocationWorkerConfig::new("test-chat", 0).is_err());
    }

    #[test]
    fn provider_failures_map_to_stable_non_sensitive_codes() {
        assert_eq!(provider_error_code(ChatProviderError::Timeout), "provider_timeout");
        assert_eq!(
            provider_error_code(ChatProviderError::RateLimited { retry_after_ms: Some(100) }),
            "provider_rate_limited"
        );
        assert_eq!(provider_error_code(ChatProviderError::Unavailable), "provider_unavailable");
        assert_eq!(provider_error_code(ChatProviderError::MalformedOutput), "provider_malformed_output");
        assert_eq!(provider_error_code(ChatProviderError::Other), "provider_other");
    }
}
