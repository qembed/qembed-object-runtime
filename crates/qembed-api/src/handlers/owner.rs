//! Authenticated tenant-owner HTTP endpoints.
//!
//! Transport parsing lives here. Authentication and persistence are delegated
//! to a composition-provided backend; this module knows no SQLx or Store types.

use std::{future::Future, pin::Pin, sync::Arc};

use crate::handlers::invocation::InvocationResponse;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use qembed_domain::{
    ids::{InvocationId, ObjectId, ObjectVersionId, TenantId},
    jcs::canonicalize_json,
};

const MAX_REQUEST_BODY_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerApiBackendError {
    Unauthorized,
    NotFound,
    ValidationRejected,
    Conflict,
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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ObjectVersionPublicationResponse {
    pub status: &'static str,
    pub object_version_id: String,
    pub snapshot_id: String,
    pub validator_version: &'static str,
    pub validation: qembed_domain::object_validation::ValidationReport,
}

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
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ObjectVersionCreationResponse, OwnerApiBackendError>>
                + Send
                + '_,
        >,
    >;
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
    >;
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

/// Builds the Owner API router with an application-owned backend.
pub fn owner_router(backend: impl OwnerApiBackend + 'static) -> Router {
    Router::new()
        .route("/owner/objects", post(create_object))
        .route(
            "/owner/objects/{object_id}/versions",
            post(create_object_version),
        )
        .route(
            "/owner/objects/{object_id}/versions/{version_id}/publish",
            post(publish_object_version),
        )
        .route(
            "/owner/invocations/{invocation_id}",
            get(inspect_invocation),
        )
        .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(OwnerApiState::new(backend))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnerCredentialError {
    InvalidTenant,
    MissingBearerToken,
}

fn owner_credentials(headers: &HeaderMap) -> Result<(TenantId, String), OwnerCredentialError> {
    let tenant_id = headers
        .get("X-Tenant-ID")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<TenantId>().ok())
        .ok_or(OwnerCredentialError::InvalidTenant)?;
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| !v.is_empty() && !v.trim().is_empty())
        .map(str::to_owned)
        .ok_or(OwnerCredentialError::MissingBearerToken)?;
    Ok((tenant_id, token))
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ObjectCreationResponse {
    pub object_id: String,
    pub state: &'static str,
}

async fn create_object(
    State(state): State<OwnerApiState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !body.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "This endpoint does not accept a request body.",
        );
    }
    let (tenant_id, token) = match owner_credentials(&headers) {
        Ok(v) => v,
        Err(error) => return credential_error(error),
    };
    match state.backend.create_object(tenant_id, token).await {
        Ok(id) => (
            StatusCode::CREATED,
            Json(ObjectCreationResponse {
                object_id: id.to_string(),
                state: "draft",
            }),
        )
            .into_response(),
        Err(error) => backend_error(error, BackendErrorContext::Ordinary),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateObjectVersionRequest {
    schema_version: i32,
    config: serde_json::Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CreateObjectVersionBodyError {
    Invalid,
}

fn parse_create_object_version_body(
    body: &[u8],
) -> Result<(u32, Vec<u8>), CreateObjectVersionBodyError> {
    let request: CreateObjectVersionRequest =
        serde_json::from_slice(body).map_err(|_| CreateObjectVersionBodyError::Invalid)?;
    canonicalize_json(body).map_err(|_| CreateObjectVersionBodyError::Invalid)?;
    if request.schema_version <= 0 {
        return Err(CreateObjectVersionBodyError::Invalid);
    }
    let schema_version =
        u32::try_from(request.schema_version).map_err(|_| CreateObjectVersionBodyError::Invalid)?;
    let config_json =
        serde_json::to_vec(&request.config).map_err(|_| CreateObjectVersionBodyError::Invalid)?;
    let canonical_config =
        canonicalize_json(&config_json).map_err(|_| CreateObjectVersionBodyError::Invalid)?;
    Ok((schema_version, canonical_config))
}

async fn create_object_version(
    State(state): State<OwnerApiState>,
    Path(object_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let object_id = match object_id.parse::<ObjectId>() {
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
        Ok(v) => v,
        Err(CreateObjectVersionBodyError::Invalid) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The request body is invalid.",
            );
        }
    };
    let (tenant_id, token) = match owner_credentials(&headers) {
        Ok(v) => v,
        Err(error) => return credential_error(error),
    };
    match state
        .backend
        .create_version(tenant_id, token, object_id, schema_version, canonical_json)
        .await
    {
        Ok(response) if response.outcome == ObjectVersionCreationOutcome::Created => {
            (StatusCode::CREATED, Json(response)).into_response()
        }
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(error) => backend_error(error, BackendErrorContext::Ordinary),
    }
}

async fn publish_object_version(
    State(state): State<OwnerApiState>,
    Path((object_id, version_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !body.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "This endpoint does not accept a request body.",
        );
    }
    let object_id = match object_id.parse::<ObjectId>() {
        Ok(id) => id,
        Err(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The object ID is invalid.",
            );
        }
    };
    let version_id = match version_id.parse::<ObjectVersionId>() {
        Ok(id) => id,
        Err(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The object version ID is invalid.",
            );
        }
    };
    let (tenant_id, token) = match owner_credentials(&headers) {
        Ok(v) => v,
        Err(error) => return credential_error(error),
    };
    match state
        .backend
        .publish_version(tenant_id, token, object_id, version_id)
        .await
    {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(error) => backend_error(error, BackendErrorContext::Publication),
    }
}

async fn inspect_invocation(
    State(state): State<OwnerApiState>,
    Path(invocation_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (tenant_id, token) = match owner_credentials(&headers) {
        Ok(v) => v,
        Err(error) => return credential_error(error),
    };
    let invocation_id = match invocation_id.parse::<InvocationId>() {
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
        Ok(Some(response)) => (StatusCode::OK, Json(response)).into_response(),
        Ok(None) => api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "The requested resource was not found.",
        ),
        Err(error) => backend_error(error, BackendErrorContext::Ordinary),
    }
}

#[derive(Clone, Copy)]
enum BackendErrorContext {
    Ordinary,
    Publication,
}

fn backend_error(error: OwnerApiBackendError, context: BackendErrorContext) -> Response {
    match (context, error) {
        (_, OwnerApiBackendError::Unauthorized) => api_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Authentication is required.",
        ),
        (_, OwnerApiBackendError::NotFound) => api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "The requested resource was not found.",
        ),
        (BackendErrorContext::Publication, OwnerApiBackendError::ValidationRejected) => api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            "The draft version failed trusted validation.",
        ),
        (BackendErrorContext::Publication, OwnerApiBackendError::Conflict) => api_error(
            StatusCode::CONFLICT,
            "conflict",
            "The object changed or is not in a publishable state.",
        ),
        (
            _,
            OwnerApiBackendError::ValidationRejected
            | OwnerApiBackendError::Conflict
            | OwnerApiBackendError::Internal,
        ) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "An unexpected internal error occurred.",
        ),
    }
}

fn credential_error(error: OwnerCredentialError) -> Response {
    match error {
        OwnerCredentialError::InvalidTenant => api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "The tenant header is invalid.",
        ),
        OwnerCredentialError::MissingBearerToken => api_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Authentication is required.",
        ),
    }
}

fn api_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (status, Json(crate::errors::ApiErrorBody { code, message })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn create_version_body_rejects_duplicate_keys_and_invalid_schema_versions() {
        assert!(
            parse_create_object_version_body(br#"{"schema_version":1,"config":{"a":1,"a":2}}"#)
                .is_err()
        );
        assert!(parse_create_object_version_body(br#"{"schema_version":0,"config":{}}"#).is_err());
        assert!(
            parse_create_object_version_body(
                br#"{"schema_version":1,"schema_version":2,"config":{}}"#
            )
            .is_err()
        );
        assert!(
            parse_create_object_version_body(br#"{"schema_version":2147483648,"config":{}}"#)
                .is_err()
        );
        assert!(
            parse_create_object_version_body(br#"{"schema_version":1.0,"config":{}}"#).is_err()
        );
    }

    #[test]
    fn create_version_body_accepts_incomplete_json_and_canonicalizes_it() {
        let (version, canonical) = parse_create_object_version_body(
            br#"{"config":{"unvalidated":true,"limits":{}},"schema_version":1}"#,
        )
        .unwrap();
        assert_eq!(version, 1);
        assert_eq!(canonical, br#"{"limits":{},"unvalidated":true}"#);
    }
}
