//! Authenticated tenant-owner HTTP endpoints.
//!
//! The tenant header is only a credential-lookup selector. A trusted Scope is
//! created exclusively after the persisted owner token has been verified.

use actix_web::{HttpRequest, HttpResponse, http::StatusCode, web};
use qembed_domain::{
    auth::{OwnerAuthenticationError, authenticate_owner},
    ids::{InvocationId, TenantId},
};
use qembed_runtime::invocation_query::InvocationQueryService;
use qembed_store::{
    auth::{OwnerAuthenticator, OwnerCredentials, OwnerAuthenticationError as StoreAuthError},
    invocation_query::PgInvocationQueryStore,
};
use sqlx::PgPool;

use crate::handlers::invocation::get_invocation;

/// Shared dependencies for Owner API routes.
#[derive(Clone)]
pub struct OwnerApiState {
    pool: PgPool,
}

impl OwnerApiState {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Registers authenticated Owner API routes.
///
/// Authentication contract:
/// - `Authorization: Bearer <owner-token>`
/// - `X-Tenant-ID: <tenant-uuid>`
///
/// The tenant header is untrusted until the token lookup succeeds.
pub fn configure_owner_routes(config: &mut web::ServiceConfig, pool: PgPool) {
    config
        .app_data(web::Data::new(OwnerApiState::new(pool)))
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
        None => return api_error(StatusCode::BAD_REQUEST, "invalid_request", "The tenant header is invalid."),
    };

    let token = match request
        .headers()
        .get(actix_web::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty() && !value.trim().is_empty())
    {
        Some(token) => token,
        None => return api_error(StatusCode::UNAUTHORIZED, "unauthorized", "Authentication is required."),
    };

    let authenticator = OwnerAuthenticator::new(state.pool.clone());
    let scope = match authenticate_owner(
        &authenticator,
        OwnerCredentials::new(tenant_id, token),
    )
    .await
    {
        Ok(scope) => scope,
        Err(OwnerAuthenticationError::Rejected(StoreAuthError::InvalidCredentials))
        | Err(OwnerAuthenticationError::NotOwner) => {
            return api_error(StatusCode::UNAUTHORIZED, "unauthorized", "Authentication is required.");
        }
        Err(OwnerAuthenticationError::Rejected(StoreAuthError::Database(_)))
        | Err(OwnerAuthenticationError::InvalidPrincipal(_))
        | Err(OwnerAuthenticationError::InvalidScope(_)) => {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", "An unexpected internal error occurred.");
        }
    };

    let invocation_id = match path.into_inner().parse::<InvocationId>() {
        Ok(id) => id,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid_request", "The invocation ID is invalid."),
    };

    let query = InvocationQueryService::new(PgInvocationQueryStore::new(state.pool.clone()));
    match get_invocation(&query, &scope, invocation_id).await {
        Ok(Some(response)) => HttpResponse::Ok().json(response),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "not_found", "The requested resource was not found."),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", "An unexpected internal error occurred."),
    }
}

fn api_error(status: StatusCode, code: &'static str, message: &'static str) -> HttpResponse {
    HttpResponse::build(status).json(crate::errors::ApiErrorBody { code, message })
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, body::to_bytes, test};

    #[actix_web::test]
    async fn owner_route_rejects_missing_tenant_header_before_database_access() {
        let pool = PgPool::connect_lazy("postgres://invalid:invalid@127.0.0.1:1/invalid")
            .expect("lazy pool configuration is valid");
        let app = test::init_service(
            App::new().configure(|config| configure_owner_routes(config, pool.clone())),
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
    async fn owner_route_rejects_missing_bearer_token_before_database_access() {
        let pool = PgPool::connect_lazy("postgres://invalid:invalid@127.0.0.1:1/invalid")
            .expect("lazy pool configuration is valid");
        let app = test::init_service(
            App::new().configure(|config| configure_owner_routes(config, pool.clone())),
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
