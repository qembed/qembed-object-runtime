//! Authenticated tenant-owner HTTP endpoints.
//!
//! Transport parsing lives here. Authentication and persistence are delegated
//! to a composition-provided backend; this module knows no SQLx or Store types.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use actix_web::{HttpRequest, HttpResponse, http::StatusCode, web};
use qembed_domain::{
    ids::{InvocationId, ObjectId, ObjectVersionId, SnapshotId, TenantId},
    jcs::canonicalize_json,
};

use crate::handlers::invocation::InvocationResponse;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerApiBackendError {
    Unauthorized,
    NotFound,
    Internal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectVersionCreationOutcome {
    Created,
    IdenticalContent,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ObjectVersionCreationResponse {
    pub outcome: ObjectVersionCreationOutcome,
    pub object_version_id: String,
    pub snapshot_id: String,
}

/// Application capability supplied by the composition root.
pub trait OwnerApiBackend: Send + Sync {
    fn create_object(
        &self,
        tenant_id: TenantId,
        owner_token: String,
    ) -> Pin<Box<dyn Future<Output = Result<ObjectId, OwnerApiBackendError>> + Send + '_>>;

    fn create_version(
        &self,
        tenant_id: TenantId,
        owner_token: String,
        object_id: ObjectId,
        schema_version: u32,
        canonical_json: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<ObjectVersionCreationResponse, OwnerApiBackendError>> + Send + '_>>;

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
        .route("/owner/objects", web::post().to(create_object))
        .route(
            "/owner/objects/{object_id}/versions",
            web::post().to(create_object_version),
        )
        .route(
            "/owner/invocations/{invocation_id}",
            web::get().to(inspect_invocation),
        );
}

fn owner_credentials(request: &HttpRequest) -> Result<(TenantId, String), HttpResponse> {
    let tenant_id = request
        .headers()
        .get("X-Tenant-ID")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<TenantId>().ok())
        .ok_or_else(|| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The tenant header is invalid.",
            )
        })?;

    let token = request
        .headers()
        .get(actix_web::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty() && !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            api_error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Authentication is required.",
            )
        })?;

    Ok((tenant_id, token))
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ObjectCreationResponse {
    pub object_id: String,
    pub state: &'static str,
}

async fn create_object(
    request: HttpRequest,
    body: web::Bytes,
    state: web::Data<OwnerApiState>,
) -> HttpResponse {
    if !body.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "This endpoint does not accept a request body.",
        );
    }

    let (tenant_id, token) = match owner_credentials(&request) {
        Ok(credentials) => credentials,
        Err(response) => return response,
    };

    match state.backend.create_object(tenant_id, token).await {
        Ok(object_id) => HttpResponse::Created().json(ObjectCreationResponse {
            object_id: object_id.to_string(),
            state: "draft",
        }),
        Err(OwnerApiBackendError::Unauthorized) => api_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Authentication is required.",
        ),
        Err(OwnerApiBackendError::NotFound) => api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "The requested resource was not found.",
        ),
        Err(OwnerApiBackendError::Internal) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "An unexpected internal error occurred.",
        ),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateObjectVersionRequest {
    schema_version: i32,
    config: serde_json::Value,
}

fn parse_create_object_version_body(body: &[u8]) -> Result<(u32, Vec<u8>), ()> {
    // Canonicalize the envelope first: the strict JCS parser rejects duplicate
    // keys anywhere in the request before serde_json can collapse them.
    let canonical_envelope = canonicalize_json(body).map_err(|_| ())?;
    let request: CreateObjectVersionRequest =
        serde_json::from_slice(&canonical_envelope).map_err(|_| ())?;
    if request.schema_version <= 0 {
        return Err(());
    }
    let schema_version = u32::try_from(request.schema_version).map_err(|_| ())?;
    let config_json = serde_json::to_vec(&request.config).map_err(|_| ())?;
    let canonical_config = canonicalize_json(&config_json).map_err(|_| ())?;
    Ok((schema_version, canonical_config))
}

async fn create_object_version(
    request: HttpRequest,
    path: web::Path<String>,
    body: web::Bytes,
    state: web::Data<OwnerApiState>,
) -> HttpResponse {
    let object_id = match path.into_inner().parse::<ObjectId>() {
        Ok(id) => id,
        Err(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The object ID is invalid.",
            );
        }
    };

    let (schema_version, canonical_json) = match parse_create_object_version_body(&body) {
        Ok(parsed) => parsed,
        Err(()) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The request body is invalid.",
            );
        }
    };

    let (tenant_id, token) = match owner_credentials(&request) {
        Ok(credentials) => credentials,
        Err(response) => return response,
    };

    match state
        .backend
        .create_version(tenant_id, token, object_id, schema_version, canonical_json)
        .await
    {
        Ok(response) if response.outcome == ObjectVersionCreationOutcome::Created => {
            HttpResponse::Created().json(response)
        }
        Ok(response) => HttpResponse::Ok().json(response),
        Err(OwnerApiBackendError::Unauthorized) => api_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Authentication is required.",
        ),
        Err(OwnerApiBackendError::NotFound) => api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "The requested resource was not found.",
        ),
        Err(OwnerApiBackendError::Internal) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "An unexpected internal error occurred.",
        ),
    }
}

async fn inspect_invocation(
    request: HttpRequest,
    path: web::Path<String>,
    state: web::Data<OwnerApiState>,
) -> HttpResponse {
    let (tenant_id, token) = match owner_credentials(&request) {
        Ok(credentials) => credentials,
        Err(response) => return response,
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
        fn create_object(
            &self,
            _: TenantId,
            _: String,
        ) -> Pin<Box<dyn Future<Output = Result<ObjectId, OwnerApiBackendError>> + Send + '_>>
        {
            Box::pin(async { panic!("backend must not be called for invalid transport input") })
        }

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

    struct FixedCreator(ObjectId);

    impl OwnerApiBackend for FixedCreator {
        fn create_object(
            &self,
            _: TenantId,
            _: String,
        ) -> Pin<Box<dyn Future<Output = Result<ObjectId, OwnerApiBackendError>> + Send + '_>>
        {
            let id = self.0;
            Box::pin(async move { Ok(id) })
        }

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
            Box::pin(async { Ok(None) })
        }
    }

    #[test]
    fn create_version_body_rejects_duplicate_keys_and_invalid_schema_versions() {
        assert!(parse_create_object_version_body(
            br#"{"schema_version":1,"config":{"a":1,"a":2}}"#
        ).is_err());
        assert!(parse_create_object_version_body(
            br#"{"schema_version":0,"config":{}}"#
        ).is_err());
        assert!(parse_create_object_version_body(
            br#"{"schema_version":1,"schema_version":2,"config":{}}"#
        ).is_err());
        assert!(parse_create_object_version_body(
            br#"{"schema_version":2147483648,"config":{}}"#
        ).is_err());
    }

    #[test]
    fn create_version_body_accepts_semantically_incomplete_json_and_canonicalizes_it() {
        let (schema_version, canonical) = parse_create_object_version_body(
            br#"{"config":{"unvalidated":true,"limits":{}},"schema_version":1}"#
        ).expect("well-formed JSON draft is accepted");
        assert_eq!(schema_version, 1);
        assert_eq!(canonical, br#"{"limits":{},"unvalidated":true}"#);
    }

    #[actix_web::test]
    async fn create_object_returns_created_draft_projection() {
        let object_id = ObjectId::new();
        let app = test::init_service(
            App::new().configure(|config| configure_owner_routes(config, FixedCreator(object_id))),
        )
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/owner/objects")
                .insert_header(("X-Tenant-ID", TenantId::new().to_string()))
                .insert_header((actix_web::http::header::AUTHORIZATION, "Bearer owner-token"))
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body()).await.expect("body reads");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("response is JSON");
        assert_eq!(json["object_id"], object_id.to_string());
        assert_eq!(json["state"], "draft");
        assert_eq!(json.as_object().expect("object response").len(), 2);
    }

    #[actix_web::test]
    async fn create_object_rejects_missing_credentials_before_backend_access() {
        let app = test::init_service(
            App::new().configure(|config| configure_owner_routes(config, NeverCalled)),
        )
        .await;

        let missing_tenant = test::call_service(
            &app,
            test::TestRequest::post().uri("/owner/objects").to_request(),
        )
        .await;
        assert_eq!(missing_tenant.status(), StatusCode::BAD_REQUEST);

        let missing_token = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/owner/objects")
                .insert_header(("X-Tenant-ID", TenantId::new().to_string()))
                .to_request(),
        )
        .await;
        assert_eq!(missing_token.status(), StatusCode::UNAUTHORIZED);
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
