//! Application composition root.
//!
//! Concrete adapters are constructed here and passed into transport handlers.
//! Handler modules do not import SQLx or concrete adapter crates.

use std::future::Future;
use std::pin::Pin;

use actix_web::web;
use qembed_domain::{
    auth::{OwnerAuthenticationError, authenticate_owner},
    ids::{InvocationId, TenantId},
};
use qembed_runtime::invocation_query::InvocationQueryService;
use qembed_store::{
    PgReadinessFailure, PgReadinessProbe,
    auth::{OwnerAuthenticator, OwnerCredentials, OwnerAuthenticationError as StoreAuthError},
    invocation_query::PgInvocationQueryStore,
};
use sqlx::PgPool;

use crate::handlers::{
    health::{
        HealthState, ReadinessProbe, ReadinessStatus, configure_health_routes as configure_routes,
    },
    invocation::{InvocationResponse, get_invocation},
    owner::{OwnerApiBackend, OwnerApiBackendError, configure_owner_routes as configure_owner},
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

impl OwnerApiBackend for PgOwnerApiBackend {
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
            let authenticator = OwnerAuthenticator::new(self.pool.clone());
            let scope = match authenticate_owner(
                &authenticator,
                OwnerCredentials::new(tenant_id, owner_token),
            )
            .await
            {
                Ok(scope) => scope,
                Err(OwnerAuthenticationError::Rejected(StoreAuthError::InvalidCredentials))
                | Err(OwnerAuthenticationError::NotOwner) => {
                    return Err(OwnerApiBackendError::Unauthorized);
                }
                Err(OwnerAuthenticationError::Rejected(StoreAuthError::Database(_)))
                | Err(OwnerAuthenticationError::InvalidPrincipal(_))
                | Err(OwnerAuthenticationError::InvalidScope(_)) => {
                    return Err(OwnerApiBackendError::Internal);
                }
            };

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
