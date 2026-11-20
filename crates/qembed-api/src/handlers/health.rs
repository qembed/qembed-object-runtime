//! Liveness and readiness HTTP endpoints.
use std::{future::Future, pin::Pin, sync::Arc};

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadinessStatus {
    Ready,
    DatabaseUnavailable,
    MigrationMismatch,
}

pub trait ReadinessProbe: Send + Sync {
    fn check(&self) -> Pin<Box<dyn Future<Output = ReadinessStatus> + Send + '_>>;
}

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

async fn liveness() -> Json<HealthBody> {
    Json(HealthBody { status: "ok" })
}

async fn readiness(State(state): State<HealthState>) -> (StatusCode, Json<HealthBody>) {
    if state.probe.check().await == ReadinessStatus::Ready {
        (StatusCode::OK, Json(HealthBody { status: "ready" }))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(HealthBody {
                status: "not_ready",
            }),
        )
    }
}

/// Builds the health router. Liveness never touches dependency state.
pub fn health_router(probe: impl ReadinessProbe + 'static) -> Router {
    Router::new()
        .route("/health/live", get(liveness))
        .route("/health/ready", get(readiness))
        .with_state(HealthState::new(probe))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedProbe(ReadinessStatus);

    impl ReadinessProbe for FixedProbe {
        fn check(&self) -> Pin<Box<dyn Future<Output = ReadinessStatus> + Send + '_>> {
            let status = self.0;
            Box::pin(async move { status })
        }
    }

    #[tokio::test]
    async fn readiness_returns_ready_when_dependencies_are_available() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let app = health_router(FixedProbe(ReadinessStatus::Ready));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .as_ref(),
            br#"{"status":"ready"}"#
        );
    }

    #[tokio::test]
    async fn migration_mismatch_returns_safe_not_ready_response() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let app = health_router(FixedProbe(ReadinessStatus::MigrationMismatch));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .as_ref(),
            br#"{"status":"not_ready"}"#
        );
    }

    #[tokio::test]
    async fn readiness_and_liveness_keep_their_safe_contracts() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let app = health_router(FixedProbe(ReadinessStatus::DatabaseUnavailable));
        let live = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(live.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(live.into_body(), usize::MAX)
                .await
                .unwrap()
                .as_ref(),
            br#"{"status":"ok"}"#
        );

        let ready = app
            .oneshot(
                Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            axum::body::to_bytes(ready.into_body(), usize::MAX)
                .await
                .unwrap()
                .as_ref(),
            br#"{"status":"not_ready"}"#
        );
    }
}
