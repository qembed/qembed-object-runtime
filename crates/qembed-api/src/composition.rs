//! Application composition root.
//!
//! Concrete adapters are constructed here and passed into transport handlers.
//! Handler modules do not import SQLx or concrete adapter crates.

use std::future::Future;
use std::pin::Pin;

use actix_web::web;
use qembed_domain::{
    auth::{OwnerAuthenticationError, authenticate_owner},
    ids::{InvocationId, ObjectId, TenantId},
    ports::DraftVersionCreation,
};
use qembed_runtime::{
    invocation_query::InvocationQueryService, object_creation::ObjectCreationService,
    object_version_creation::ObjectVersionCreationService,
};
use qembed_store::{
    PgReadinessFailure, PgReadinessProbe,
    auth::{OwnerAuthenticationError as StoreAuthError, OwnerAuthenticator, OwnerCredentials},
    invocation_query::PgInvocationQueryStore,
    object_repository::ObjectRepository,
};
use sqlx::PgPool;

use crate::handlers::{
    health::{
        HealthState, ReadinessProbe, ReadinessStatus, configure_health_routes as configure_routes,
    },
    invocation::{InvocationResponse, get_invocation},
    owner::{
        ObjectVersionCreationOutcome, ObjectVersionCreationResponse, OwnerApiBackend,
        OwnerApiBackendError, configure_owner_routes as configure_owner,
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
pub fn configure_health_routes(config: &mut web::ServiceConfig, probe: PgReadinessProbe) {
    config.app_data(web::Data::new(HealthState::new(StoreReadinessProbe(probe))));
    configure_routes(config);
}

struct PgOwnerApiBackend {
    pool: PgPool,
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
pub fn configure_owner_routes(config: &mut web::ServiceConfig, pool: PgPool) {
    configure_owner(config, PgOwnerApiBackend { pool });
}
