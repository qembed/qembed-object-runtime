//! Application composition root.
//!
//! Concrete adapters are constructed here and passed into transport handlers.
//! Handler modules do not import SQLx or concrete adapter crates.

use actix_web::web;
use qembed_store::{PgReadinessFailure, PgReadinessProbe};

use crate::handlers::health::{
    HealthState, ReadinessProbe, ReadinessStatus, configure_health_routes as configure_routes,
};
use std::future::Future;
use std::pin::Pin;

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

/// Installs the store-backed readiness state and registers liveness/readiness
/// routes. The supplied probe must use the same pool as the application store.
pub fn configure_health_routes(config: &mut web::ServiceConfig, probe: PgReadinessProbe) {
    config.app_data(web::Data::new(HealthState::new(StoreReadinessProbe(probe))));
    configure_routes(config);
}
