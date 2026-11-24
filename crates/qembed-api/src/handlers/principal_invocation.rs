//! Tenant-Principal invocation endpoint. Authentication is token-based and
//! tenant identity is resolved from the canonical Principal registry.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use qembed_domain::{
    ids::{CallerId, InvocationId, ObjectId},
    jcs::canonicalize_json,
};

use crate::handlers::owner::{InvocationStartResponse, OwnerApiBackendError};

const MAX_REQUEST_BODY_BYTES: usize = 256 * 1024;
const MAX_INPUT_BYTES: usize = 65_536;

pub trait PrincipalInvocationBackend: Send + Sync {
    fn invoke(
        &self,
        principal_token: String,
        object_id: ObjectId,
        caller_id: CallerId,
        idempotency_key: String,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<InvocationId, OwnerApiBackendError>> + Send + '_>>;
}

#[derive(Clone)]
struct PrincipalInvocationState {
    backend: Arc<dyn PrincipalInvocationBackend>,
}

impl PrincipalInvocationState {
    fn new(backend: impl PrincipalInvocationBackend + 'static) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }
}

pub fn principal_invocation_router(backend: impl PrincipalInvocationBackend + 'static) -> Router {
    Router::new()
        .route("/v1/objects/{object_id}/invocations", post(invoke_object))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(PrincipalInvocationState::new(backend))
}

fn bearer_token(headers: &HeaderMap) -> Result<String, ()> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or(())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InvokeObjectRequest {
    caller_id: String,
    idempotency_key: String,
    input: serde_json::Value,
}

fn parse_invoke_body(body: &[u8]) -> Result<(CallerId, String, serde_json::Value), ()> {
    canonicalize_json(body).map_err(|_| ())?;
    let request: InvokeObjectRequest = serde_json::from_slice(body).map_err(|_| ())?;
    let caller_id = request.caller_id.parse::<CallerId>().map_err(|_| ())?;
    let input_bytes = serde_json::to_vec(&request.input).map_err(|_| ())?;
    let canonical_input = canonicalize_json(&input_bytes).map_err(|_| ())?;
    if canonical_input.len() > MAX_INPUT_BYTES || request.idempotency_key.trim().is_empty() {
        return Err(());
    }
    Ok((caller_id, request.idempotency_key, request.input))
}

async fn invoke_object(
    State(state): State<PrincipalInvocationState>,
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
    let token = match bearer_token(&headers) {
        Ok(token) => token,
        Err(()) => {
            return api_error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Authentication is required.",
            );
        }
    };
    let (caller_id, idempotency_key, input) = match parse_invoke_body(&body) {
        Ok(request) => request,
        Err(()) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The invocation request body is invalid.",
            );
        }
    };

    match state
        .backend
        .invoke(token, object_id, caller_id, idempotency_key, input)
        .await
    {
        Ok(invocation_id) => (
            StatusCode::ACCEPTED,
            Json(InvocationStartResponse {
                invocation_id: invocation_id.to_string(),
                status: "accepted",
            }),
        )
            .into_response(),
        Err(error) => backend_error(error),
    }
}

fn backend_error(error: OwnerApiBackendError) -> Response {
    match error {
        OwnerApiBackendError::Unauthorized => api_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Authentication is required.",
        ),
        OwnerApiBackendError::NotFound => api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "The requested resource was not found.",
        ),
        OwnerApiBackendError::ValidationRejected | OwnerApiBackendError::Internal => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "An unexpected internal error occurred.",
        ),
        OwnerApiBackendError::Conflict => api_error(
            StatusCode::CONFLICT,
            "conflict",
            "The invocation conflicts with the current object or idempotency state.",
        ),
        OwnerApiBackendError::InvalidRequest => api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "The invocation request is invalid.",
        ),
        OwnerApiBackendError::RateLimited { retry_after } => {
            let mut response = api_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "The request rate limit has been reached.",
            );
            if let Some(delay) = retry_after
                && let Ok(value) = HeaderValue::from_str(&delay.as_secs().max(1).to_string())
            {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
    }
}

fn api_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (status, Json(crate::errors::ApiErrorBody { code, message })).into_response()
}
