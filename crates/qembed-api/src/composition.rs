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
    PgReadinessFailure, PgReadinessProbe,
    auth::{OwnerAuthenticationError as StoreAuthError, OwnerAuthenticator, OwnerCredentials},
    invocation_query::PgInvocationQueryStore,
    invocation_start::PgInvocationStartStore,
    object_repository::ObjectRepository,
    outbox_dispatcher::PgOutboxDispatcher,
};
use sqlx::PgPool;

use crate::adapters::{nats_jetstream_publisher::NatsJetStreamPublisher, valkey_rate_limit::ValkeyRateLimitStore};

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
    rate_limiter: RateLimitService<ValkeyRateLimitStore>,
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

impl OwnerApiBackend for PgOwnerApiBackend {
    fn create_object(
        &self,
        tenant_id: TenantId,
        owner_token: String,
    ) -> Pin<Box<dyn Future<Output = Result<ObjectId, OwnerApiBackendError>> + Send + '_>> {
        Box::pin(async move {
            let scope = self.authenticate(tenant_id, owner_token).await?;
            let service = ObjectCreationService::new(ObjectRepository::new(self.pool.clone()));
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
                ObjectVersionCreationService::new(ObjectRepository::new(self.pool.clone()));
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
            let service = ObjectPublicationService::new(ObjectRepository::new(self.pool.clone()));
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
            let key = RateLimitKey::for_owner(&scope, object_id)
                .map_err(|_| OwnerApiBackendError::Internal)?;
            match self.rate_limiter.check(
                &key,
                self.rate_limit_policies.for_class(RateLimitClass::Owner),
                1,
            ).await {
                Ok(decision) if !decision.allowed() => return Err(OwnerApiBackendError::RateLimited),
                Ok(_) => {}
                Err(error) => {
                    // Never substitute per-process counters when shared KV is unavailable.
                    // The selected practical policy is fail-open for ordinary invocations.
                    eprintln!("distributed rate limiter unavailable; allowing request: {error}");
                }
            }
            let service =
                InvocationStartService::new(PgInvocationStartStore::new(self.pool.clone()));
            match service
                .start(
                    &scope,
                    InvocationStartRequest {
                        object_id,
                        caller_id,
                        idempotency_key,
                        input,
                    },
                )
                .await
            {
                Ok(result) => match result {
                    qembed_runtime::invocation_start::InvocationStartResult::Accepted {
                        invocation_id,
                    }
                    | qembed_runtime::invocation_start::InvocationStartResult::Replayed {
                        invocation_id,
                    } => Ok(invocation_id),
                },
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
            let query = InvocationQueryService::new(PgInvocationQueryStore::new(self.pool.clone()));
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
    valkey: ValkeyRateLimitStore,
    policies: RateLimitPolicies,
) -> Router {
    configure_owner(PgOwnerApiBackend {
        pool,
        rate_limiter: RateLimitService::new(valkey),
        rate_limit_policies: policies,
    })
}

/// Composes the HTTP surface without creating a listener or changing runtime lifecycle.
pub fn api_router(
    pool: PgPool,
    probe: PgReadinessProbe,
    valkey: ValkeyRateLimitStore,
    policies: RateLimitPolicies,
) -> Router {
    configure_health_routes(probe).merge(configure_owner_routes(pool, valkey, policies))
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
