//! Authenticated tenant-owner HTTP endpoints.
//!
//! Transport parsing lives here. Authentication and persistence are delegated
//! to a composition-provided backend; this module knows no SQLx or Store types.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use actix_web::{HttpRequest, HttpResponse, http::StatusCode, web};
use qembed_domain::ids::{InvocationId, TenantId};

use crate::handlers::invocation::InvocationResponse;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerApiBackendError {
    Unauthorized,
    Internal,
}

/// Application capability supplied by the composition root.
pub trait OwnerApiBackend: Send + Sync {
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
    >;
}

#[derive(Clone)]
pub struct OwnerApiState {
    backend: Arc<dyn OwnerApiBackend>,
}

impl OwnerApiState {
    #[must_use]
    pub fn new(backend: impl OwnerApiBackend + 'static) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }
}

/// Registers Owner routes with an application-owned backend.
pub fn configure_owner_routes(
    config: &mut web::ServiceConfig,
    backend: impl OwnerApiBackend + 'static,
) {
    config
        .app_data(web::Data::new(OwnerApiState::new(backend)))
        .route(
            "/owner/invocations/{invocation_id}",
            web::get().to(inspect_invocation),
        );
}

async fn inspect_invocation(
    request: HttpRequest,
    path: web::Path<String>,
    state: web::Data<OwnerApiState>,
) -> HttpResponse {
    let tenant_id = match request
        .headers()
        .get("X-Tenant-ID")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<TenantId>().ok())
    {
        Some(id) => id,
        None => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The tenant header is invalid.",
            );
        }
    };

    let token = match request
        .headers()
        .get(actix_web::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty() && !value.trim().is_empty())
    {
        Some(token) => token.to_owned(),
        None => {
            return api_error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Authentication is required.",
            );
        }
    };

    let invocation_id = match path.into_inner().parse::<InvocationId>() {
        Ok(id) => id,
        Err(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The invocation ID is invalid.",
            );
        }
    };

    match state.backend.inspect(tenant_id, token, invocation_id).await {
        Ok(Some(response)) => HttpResponse::Ok().json(response),
        Ok(None) => api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "The requested resource was not found.",
        ),
        Err(OwnerApiBackendError::Unauthorized) => api_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Authentication is required.",
        ),
        Err(OwnerApiBackendError::Internal) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "An unexpected internal error occurred.",
        ),
    }
}

fn api_error(status: StatusCode, code: &'static str, message: &'static str) -> HttpResponse {
    HttpResponse::build(status).json(crate::errors::ApiErrorBody { code, message })
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, body::to_bytes, test};

    struct NeverCalled;
    impl OwnerApiBackend for NeverCalled {
        fn inspect(
            &self,
            _: TenantId,
            _: String,
            _: InvocationId,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Option<InvocationResponse>, OwnerApiBackendError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async { panic!("backend must not be called for invalid transport input") })
        }
    }

    #[actix_web::test]
    async fn owner_route_rejects_missing_tenant_header_before_backend_access() {
        let app = test::init_service(
            App::new().configure(|config| configure_owner_routes(config, NeverCalled)),
        )
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/owner/invocations/00000000-0000-4000-8000-000000000001")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body()).await.expect("body reads");
        assert!(String::from_utf8_lossy(&body).contains("invalid_request"));
    }

    #[actix_web::test]
    async fn owner_route_rejects_missing_bearer_token_before_backend_access() {
        let app = test::init_service(
            App::new().configure(|config| configure_owner_routes(config, NeverCalled)),
        )
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/owner/invocations/00000000-0000-4000-8000-000000000001")
                .insert_header(("X-Tenant-ID", TenantId::new().to_string()))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
