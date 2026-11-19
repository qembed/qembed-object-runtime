//! Liveness and readiness HTTP endpoints.
//!
//! Liveness reports only that the process can serve this handler. Readiness
//! additionally checks required dependencies through a composition-provided
//! probe. Both response bodies are fixed and contain no infrastructure details.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use actix_web::{HttpResponse, get, web};
use serde::Serialize;

/// Internal readiness result. Failure variants are deliberately not serialized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadinessStatus {
    Ready,
    DatabaseUnavailable,
    MigrationMismatch,
}

/// Infrastructure probe supplied by the API composition root.
pub trait ReadinessProbe: Send + Sync {
    fn check(&self) -> Pin<Box<dyn Future<Output = ReadinessStatus> + Send + '_>>;
}

/// Shared state for the readiness endpoint.
#[derive(Clone)]
pub struct HealthState {
    probe: Arc<dyn ReadinessProbe>,
}

impl HealthState {
    #[must_use]
    pub fn new(probe: impl ReadinessProbe + 'static) -> Self {
        Self {
            probe: Arc::new(probe),
        }
    }
}

#[derive(Serialize)]
struct HealthBody {
    status: &'static str,
}

/// GET /health/live
///
/// This endpoint intentionally does not query PostgreSQL. A dependency outage
/// must not cause the process liveness probe to restart a healthy process.
#[get("/health/live")]
pub async fn liveness() -> HttpResponse {
    HttpResponse::Ok().json(HealthBody { status: "ok" })
}

/// GET /health/ready
///
/// Returns 503 when a required dependency is unavailable or migration history
/// does not match this binary. The public response intentionally does not say
/// which check failed and never includes SQLx diagnostics or connection data.
#[get("/health/ready")]
pub async fn readiness(state: web::Data<HealthState>) -> HttpResponse {
    if state.probe.check().await == ReadinessStatus::Ready {
        HttpResponse::Ok().json(HealthBody { status: "ready" })
    } else {
        HttpResponse::ServiceUnavailable().json(HealthBody {
            status: "not_ready",
        })
    }
}

/// Registers the health routes. The caller must install HealthState as app data.
pub fn configure_health_routes(config: &mut web::ServiceConfig) {
    config.service(liveness).service(readiness);
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, body::to_bytes, http::StatusCode, test};

    struct FixedProbe(ReadinessStatus);

    impl ReadinessProbe for FixedProbe {
        fn check(&self) -> Pin<Box<dyn Future<Output = ReadinessStatus> + Send + '_>> {
            let status = self.0;
            Box::pin(async move { status })
        }
    }

    #[actix_web::test]
    async fn liveness_is_independent_of_dependency_state() {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(HealthState::new(FixedProbe(
                    ReadinessStatus::DatabaseUnavailable,
                ))))
                .configure(configure_health_routes),
        )
        .await;

        let response = test::call_service(
            &app,
            test::TestRequest::get().uri("/health/live").to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body()).await.unwrap().as_ref(),
            br#"{"status":"ok"}"#
        );
    }

    #[actix_web::test]
    async fn readiness_returns_ok_when_database_and_migrations_are_ready() {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(HealthState::new(FixedProbe(
                    ReadinessStatus::Ready,
                ))))
                .configure(configure_health_routes),
        )
        .await;

        let response = test::call_service(
            &app,
            test::TestRequest::get().uri("/health/ready").to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body()).await.unwrap().as_ref(),
            br#"{"status":"ready"}"#
        );
    }

    #[actix_web::test]
    async fn readiness_returns_safe_503_for_database_outage() {
        assert_not_ready(ReadinessStatus::DatabaseUnavailable).await;
    }

    #[actix_web::test]
    async fn readiness_returns_safe_503_for_migration_mismatch() {
        assert_not_ready(ReadinessStatus::MigrationMismatch).await;
    }

    async fn assert_not_ready(status: ReadinessStatus) {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(HealthState::new(FixedProbe(status))))
                .configure(configure_health_routes),
        )
        .await;

        let response = test::call_service(
            &app,
            test::TestRequest::get().uri("/health/ready").to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body()).await.unwrap();
        assert_eq!(body.as_ref(), br#"{"status":"not_ready"}"#);
        let body = String::from_utf8(body.to_vec()).unwrap().to_lowercase();
        for forbidden in ["postgres", "sqlx", "checksum", "password", "database_url"] {
            assert!(!body.contains(forbidden));
        }
    }
}
