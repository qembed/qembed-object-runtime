//! Application composition root.
//!
//! Concrete adapters are constructed here and passed into transport handlers.
//! Handler modules do not import SQLx or concrete adapter crates.

use std::future::Future;
use std::pin::Pin;

use axum::Router;
use qembed_domain::{
    auth::{OwnerAuthenticationError, authenticate_owner},
    ids::{CallerId, InvocationId, ObjectId, ObjectVersionId, TenantId},
    ports::DraftVersionCreation,
    rate_limit::{RateLimitClass, RateLimitKey, RateLimitPolicies},
};
use qembed_runtime::{
    invocation_query::InvocationQueryService,
    invocation_start::{InvocationStartError, InvocationStartRequest, InvocationStartService},
    object_creation::ObjectCreationService,
    object_publication::{ObjectPublicationError, ObjectPublicationService},
    object_version_creation::ObjectVersionCreationService,
    outbox_relay::{OutboxRelay, OutboxRelayConfig},
    rate_limit::RateLimitService,
};
use qembed_store::{
    PgContext, PgReadinessFailure, PgReadinessProbe,
    auth::{OwnerAuthenticationError as StoreAuthError, OwnerAuthenticator, OwnerCredentials},
    invocation_query::PgInvocationQueryStore,
    invocation_start::PgInvocationStartStore,
    object_repository::ObjectRepository,
    outbox_dispatcher::PgOutboxDispatcher,
    valkey_rate_limit::ValkeyRateLimitStore,
};
use sqlx::PgPool;

use crate::adapters::nats_jetstream_publisher::NatsJetStreamPublisher;

use crate::handlers::{
    health::{ReadinessProbe, ReadinessStatus, health_router as configure_routes},
    invocation::{InvocationResponse, get_invocation},
    owner::{
        ObjectVersionCreationOutcome, ObjectVersionCreationResponse,
        ObjectVersionPublicationResponse, OwnerApiBackend, OwnerApiBackendError,
        owner_router as configure_owner,
    },
};

struct StoreReadinessProbe(PgReadinessProbe);

impl ReadinessProbe for StoreReadinessProbe {
    fn check(&self) -> Pin<Box<dyn Future<Output = ReadinessStatus> + Send + '_>> {
        Box::pin(async move {
            match self.0.check().await {
                Ok(()) => ReadinessStatus::Ready,
                Err(PgReadinessFailure::DatabaseUnavailable) => {
                    ReadinessStatus::DatabaseUnavailable
                }
                Err(PgReadinessFailure::MigrationMismatch) => ReadinessStatus::MigrationMismatch,
            }
        })
    }
}

/// Installs the store-backed readiness state and registers liveness/readiness routes.
pub fn configure_health_routes(probe: PgReadinessProbe) -> Router {
    configure_routes(StoreReadinessProbe(probe))
}

struct PgOwnerApiBackend {
    pool: PgPool,
    context: PgContext,
    rate_limit: RateLimitService<ValkeyRateLimitStore>,
    rate_limit_policies: RateLimitPolicies,
}

impl PgOwnerApiBackend {
    async fn authenticate(
        &self,
        tenant_id: TenantId,
        owner_token: String,
    ) -> Result<qembed_domain::scope::Scope, OwnerApiBackendError> {
        let authenticator = OwnerAuthenticator::new(self.pool.clone());
        authenticate_owner(
            &authenticator,
            OwnerCredentials::new(tenant_id, owner_token),
        )
        .await
        .map_err(|error| match error {
            OwnerAuthenticationError::Rejected(StoreAuthError::InvalidCredentials)
            | OwnerAuthenticationError::NotOwner => OwnerApiBackendError::Unauthorized,
            OwnerAuthenticationError::Rejected(StoreAuthError::Database(_))
            | OwnerAuthenticationError::InvalidPrincipal(_)
            | OwnerAuthenticationError::InvalidScope(_) => OwnerApiBackendError::Internal,
        })
    }
}

#[derive(Clone)]
struct PgPrincipalApiBackend {
    pool: PgPool,
    context: PgContext,
    rate_limit: RateLimitService<ValkeyRateLimitStore>,
    rate_limit_policies: RateLimitPolicies,
}

impl PrincipalInvocationBackend for PgPrincipalApiBackend {
    fn invoke(
        &self,
        principal_token: String,
        object_id: ObjectId,
        caller_id: CallerId,
        idempotency_key: String,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<InvocationId, OwnerApiBackendError>> + Send + '_>> {
        Box::pin(async move {
            let authenticator = PrincipalAuthenticator::new(self.pool.clone());
            let scope =
                authenticate_principal(&authenticator, PrincipalCredentials::new(principal_token))
                    .await
                    .map_err(|error| match error {
                        DomainPrincipalAuthError::Rejected(
                            StorePrincipalAuthError::InvalidCredentials,
                        ) => OwnerApiBackendError::Unauthorized,
                        _ => OwnerApiBackendError::Internal,
                    })?;

            let (rate_limit_key, rate_limit_class) = match scope.principal().kind() {
                PrincipalKind::TenantOwner => (
                    RateLimitKey::for_owner(&scope, object_id)
                        .map_err(|_| OwnerApiBackendError::Internal)?,
                    RateLimitClass::Owner,
                ),
                PrincipalKind::Authenticated => (
                    RateLimitKey::for_authenticated(&scope, object_id)
                        .map_err(|_| OwnerApiBackendError::Internal)?,
                    RateLimitClass::Authenticated,
                ),
            };
            let policy = self.rate_limit_policies.for_class(rate_limit_class);
            let service =
                InvocationStartService::new(PgInvocationStartStore::new(self.context.clone()));
            match service
                .start_rate_limited(
                    &scope,
                    InvocationStartRequest {
                        object_id,
                        caller_id,
                        idempotency_key,
                        input,
                    },
                    &self.rate_limit,
                    &rate_limit_key,
                    policy,
                    1,
                )
                .await
            {
                Ok(qembed_runtime::invocation_start::InvocationStartResult::Accepted {
                    invocation_id,
                    rate_limit_degraded,
                }) => {
                    if rate_limit_degraded {
                        eprintln!(
                            "event=rate_limit_fail_open class={} object_id={object_id}",
                            rate_limit_class.as_str()
                        );
                    }
                    Ok(invocation_id)
                }
                Ok(qembed_runtime::invocation_start::InvocationStartResult::Replayed {
                    invocation_id,
                }) => Ok(invocation_id),
                Err(InvocationStartError::RateLimited { retry_after }) => {
                    Err(OwnerApiBackendError::RateLimited { retry_after })
                }
                Err(InvocationStartError::IdempotencyConflict) => {
                    Err(OwnerApiBackendError::Conflict)
                }
                Err(InvocationStartError::InvalidIdempotencyKey(_)) => {
                    Err(OwnerApiBackendError::InvalidRequest)
                }
                Err(InvocationStartError::ObjectNotFound) => {
                    Err(OwnerApiBackendError::NotFound)
                }
                Err(
                    InvocationStartError::ObjectNotActive
                    | InvocationStartError::NoPublishedVersion,
                ) => Err(OwnerApiBackendError::Conflict),
                Err(InvocationStartError::Store(
                    qembed_store::begin_invocation::BeginInvocationError::CallerResolution(
                        qembed_store::caller::CallerResolutionError::NotDelegated,
                    ),
                )) => Err(OwnerApiBackendError::NotFound),
                Err(InvocationStartError::Store(
                    qembed_store::begin_invocation::BeginInvocationError::Idempotency(
                        qembed_store::idempotency::IdempotencyReservationError::PayloadMismatch,
                    ),
                )) => Err(OwnerApiBackendError::Conflict),
                Err(InvocationStartError::Store(
                    qembed_store::begin_invocation::BeginInvocationError::NoPublishedVersion
                    | qembed_store::begin_invocation::BeginInvocationError::ObjectNotActive,
                )) => Err(OwnerApiBackendError::Conflict),
                Err(InvocationStartError::Store(
                    qembed_store::begin_invocation::BeginInvocationError::ObjectNotFound,
                )) => Err(OwnerApiBackendError::NotFound),
                Err(_) => Err(OwnerApiBackendError::Internal),
            }
        })
    }
}

impl OwnerApiBackend for PgOwnerApiBackend {
    fn create_object(
        &self,
        tenant_id: TenantId,
        owner_token: String,
    ) -> Pin<Box<dyn Future<Output = Result<ObjectId, OwnerApiBackendError>> + Send + '_>> {
        Box::pin(async move {
            let scope = self.authenticate(tenant_id, owner_token).await?;
            let service = ObjectCreationService::new(ObjectRepository::new(self.context.clone()));
            service
                .create(&scope)
                .await
                .map_err(|_| OwnerApiBackendError::Internal)
        })
    }

    fn create_version(
        &self,
        tenant_id: TenantId,
        owner_token: String,
        object_id: ObjectId,
        schema_version: u32,
        canonical_json: Vec<u8>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ObjectVersionCreationResponse, OwnerApiBackendError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let scope = self.authenticate(tenant_id, owner_token).await?;
            let service =
                ObjectVersionCreationService::new(ObjectRepository::new(self.context.clone()));
            match service
                .create(&scope, object_id, schema_version, canonical_json)
                .await
            {
                Ok(DraftVersionCreation::Created {
                    version_id,
                    snapshot_id,
                }) => Ok(ObjectVersionCreationResponse {
                    outcome: ObjectVersionCreationOutcome::Created,
                    object_version_id: version_id.to_string(),
                    snapshot_id: snapshot_id.to_string(),
                }),
                Ok(DraftVersionCreation::IdenticalContent {
                    version_id,
                    snapshot_id,
                }) => Ok(ObjectVersionCreationResponse {
                    outcome: ObjectVersionCreationOutcome::IdenticalContent,
                    object_version_id: version_id.to_string(),
                    snapshot_id: snapshot_id.to_string(),
                }),
                Err(
                    qembed_runtime::object_version_creation::ObjectVersionCreationError::Store(
                        qembed_store::object_repository::ObjectRepositoryError::ObjectNotFound,
                    ),
                ) => Err(OwnerApiBackendError::NotFound),
                Err(_) => Err(OwnerApiBackendError::Internal),
            }
        })
    }

    fn publish_version(
        &self,
        tenant_id: TenantId,
        owner_token: String,
        object_id: ObjectId,
        version_id: ObjectVersionId,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ObjectVersionPublicationResponse, OwnerApiBackendError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let scope = self.authenticate(tenant_id, owner_token).await?;
            let service =
                ObjectPublicationService::new(ObjectRepository::new(self.context.clone()));
            match service.publish(&scope, object_id, version_id).await {
                Ok(result) => {
                    let report = *result.report();
                    Ok(ObjectVersionPublicationResponse {
                        status: "published",
                        object_version_id: result.object_version_id().to_string(),
                        snapshot_id: result.snapshot_id().to_string(),
                        validator_version: report.validator_version(),
                        validation: report,
                    })
                }
                Err(ObjectPublicationError::NotFound) => Err(OwnerApiBackendError::NotFound),
                Err(ObjectPublicationError::ValidationRejected(_)) => Err(OwnerApiBackendError::ValidationRejected),
                Err(ObjectPublicationError::InvalidObjectState) => Err(OwnerApiBackendError::Conflict),
                Err(ObjectPublicationError::Store(
                    qembed_store::object_repository::ObjectRepositoryError::ObjectNotFound
                    | qembed_store::object_repository::ObjectRepositoryError::VersionNotFound
                    | qembed_store::object_repository::ObjectRepositoryError::SnapshotNotFound,
                )) => Err(OwnerApiBackendError::NotFound),
                Err(ObjectPublicationError::Store(
                    qembed_store::object_repository::ObjectRepositoryError::InvalidObjectState
                    | qembed_store::object_repository::ObjectRepositoryError::PublishedVersionMismatch
                    | qembed_store::object_repository::ObjectRepositoryError::VersionMustBeDraft
                    | qembed_store::object_repository::ObjectRepositoryError::CurrentVersionNotPublished,
                )) => Err(OwnerApiBackendError::Conflict),
                Err(ObjectPublicationError::Store(_)) => Err(OwnerApiBackendError::Internal),
            }
        })
    }

    fn invoke(
        &self,
        tenant_id: TenantId,
        owner_token: String,
        object_id: ObjectId,
        caller_id: CallerId,
        idempotency_key: String,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<InvocationId, OwnerApiBackendError>> + Send + '_>> {
        Box::pin(async move {
            let scope = self.authenticate(tenant_id, owner_token).await?;
            let service =
                InvocationStartService::new(PgInvocationStartStore::new(self.context.clone()));
            let rate_limit_key = RateLimitKey::for_owner(&scope, object_id)
                .map_err(|_| OwnerApiBackendError::Internal)?;
            let policy = self.rate_limit_policies.for_class(RateLimitClass::Owner);
            match service
                .start_rate_limited(
                    &scope,
                    InvocationStartRequest {
                        object_id,
                        caller_id,
                        idempotency_key,
                        input,
                    },
                    &self.rate_limit,
                    &rate_limit_key,
                    policy,
                    1,
                )
                .await
            {
                Ok(qembed_runtime::invocation_start::InvocationStartResult::Accepted {
                    invocation_id,
                    rate_limit_degraded,
                }) => {
                    if rate_limit_degraded {
                        // Structured fallback signal: Valkey is admission-only;
                        // durable invocation and budget accounting remain in PostgreSQL.
                        eprintln!("event=rate_limit_fail_open class=owner object_id={object_id}");
                    }
                    Ok(invocation_id)
                }
                Ok(qembed_runtime::invocation_start::InvocationStartResult::Replayed {
                    invocation_id,
                }) => Ok(invocation_id),
                Err(InvocationStartError::RateLimited { retry_after }) => {
                    Err(OwnerApiBackendError::RateLimited { retry_after })
                }
                Err(InvocationStartError::IdempotencyConflict) => {
                    Err(OwnerApiBackendError::Conflict)
                }
                Err(InvocationStartError::InvalidIdempotencyKey(_)) => {
                    Err(OwnerApiBackendError::InvalidRequest)
                }
                Err(InvocationStartError::Store(
                    qembed_store::begin_invocation::BeginInvocationError::CallerResolution(
                        qembed_store::caller::CallerResolutionError::NotDelegated,
                    ),
                )) => Err(OwnerApiBackendError::NotFound),
                Err(InvocationStartError::Store(
                    qembed_store::begin_invocation::BeginInvocationError::Idempotency(
                        qembed_store::idempotency::IdempotencyReservationError::PayloadMismatch,
                    ),
                )) => Err(OwnerApiBackendError::Conflict),
                Err(InvocationStartError::Store(
                    qembed_store::begin_invocation::BeginInvocationError::NoPublishedVersion
                    | qembed_store::begin_invocation::BeginInvocationError::ObjectNotActive,
                )) => Err(OwnerApiBackendError::Conflict),
                Err(InvocationStartError::Store(
                    qembed_store::begin_invocation::BeginInvocationError::ObjectNotFound,
                )) => Err(OwnerApiBackendError::NotFound),
                Err(_) => Err(OwnerApiBackendError::Internal),
            }
        })
    }

    fn inspect(
        &self,
        tenant_id: TenantId,
        owner_token: String,
        invocation_id: InvocationId,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<InvocationResponse>, OwnerApiBackendError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let scope = self.authenticate(tenant_id, owner_token).await?;
            let query =
                InvocationQueryService::new(PgInvocationQueryStore::new(self.context.clone()));
            get_invocation(&query, &scope, invocation_id)
                .await
                .map_err(|_| OwnerApiBackendError::Internal)
        })
    }
}

/// Installs PostgreSQL-backed Owner routes. Authentication and SQL adapters are
/// wired here, keeping concrete infrastructure out of HTTP handlers.
pub fn configure_owner_routes(
    pool: PgPool,
    context: PgContext,
    rate_limit_store: ValkeyRateLimitStore,
    rate_limit_policies: RateLimitPolicies,
) -> Router {
    configure_owner(PgOwnerApiBackend {
        pool,
        context,
        rate_limit: RateLimitService::new(rate_limit_store),
        rate_limit_policies,
    })
}

/// Composes the HTTP surface with mandatory distributed Owner invocation admission.
pub fn api_router(
    pool: PgPool,
    context: PgContext,
    probe: PgReadinessProbe,
    rate_limit_store: ValkeyRateLimitStore,
    rate_limit_policies: RateLimitPolicies,
) -> Router {
    let principal_backend = PgPrincipalApiBackend {
        pool: pool.clone(),
        context: context.clone(),
        rate_limit: RateLimitService::new(rate_limit_store.clone()),
        rate_limit_policies,
    };
    configure_health_routes(probe)
        .merge(configure_owner_routes(
            pool,
            context,
            rate_limit_store,
            rate_limit_policies,
        ))
        .merge(principal_invocation_router(principal_backend))
}

/// Composes the PostgreSQL Outbox Relay with its JetStream publisher.
///
/// The returned service is transport-neutral at the runtime boundary; this
/// function is the only place that wires the PostgreSQL and NATS adapters.
pub fn compose_outbox_relay(
    pool: PgPool,
    publisher: NatsJetStreamPublisher,
    config: OutboxRelayConfig,
) -> OutboxRelay<PgOutboxDispatcher, NatsJetStreamPublisher> {
    OutboxRelay::new(PgOutboxDispatcher::new(pool), publisher, config)
}
