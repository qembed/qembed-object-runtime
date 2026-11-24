//! Public Anonymous Object invocation endpoint.
///
/// The route deliberately accepts no Principal or Caller ID. It derives a
/// subject fingerprint from the direct TCP peer address with a server-held
/// HMAC key. Forwarding headers are not trusted by this handler.
use std::{
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, FromRequestParts, Path, State},
    http::{HeaderValue, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
    routing::post,
};
use qembed_domain::{
    ids::{InvocationId, ObjectId, TenantId},
    jcs::canonicalize_json,
};
use ring::hmac;

use crate::handlers::owner::{InvocationStartResponse, OwnerApiBackendError};

const MAX_REQUEST_BODY_BYTES: usize = 256 * 1024;
const MAX_INPUT_BYTES: usize = 65_536;
const FINGERPRINT_DOMAIN: &[u8] = b"qembed:anonymous-peer-fingerprint:v1:";

/// Server-held HMAC key used to derive per-Object Anonymous subject fingerprints.
///
/// The key is intentionally not Debug or serializable. A minimum 32-byte secret
/// is required. Rotation changes the fingerprint namespace and therefore
/// starts a fresh rate-limit/idempotency identity for each peer.
#[derive(Clone)]
pub struct AnonymousFingerprintKey {
    key: Arc<hmac::Key>,
}

impl AnonymousFingerprintKey {
    pub fn from_secret(secret: &[u8]) -> Result<Self, AnonymousFingerprintKeyError> {
        if secret.len() < 32 {
            return Err(AnonymousFingerprintKeyError::SecretTooShort);
        }
        Ok(Self {
            key: Arc::new(hmac::Key::new(hmac::HMAC_SHA256, secret)),
        })
    }

    /// Derives an Object-scoped pseudonymous fingerprint from the direct peer
    /// IP. The peer port is excluded so reconnects retain the same bucket.
    #[must_use]
    pub fn fingerprint(
        &self,
        peer: SocketAddr,
        tenant_id: TenantId,
        object_id: ObjectId,
    ) -> [u8; 32] {
        let mut material = Vec::with_capacity(FINGERPRINT_DOMAIN.len() + 16 + 16 + 17);
        material.extend_from_slice(FINGERPRINT_DOMAIN);
        material.extend_from_slice(tenant_id.as_uuid().as_bytes());
        material.extend_from_slice(object_id.as_uuid().as_bytes());
        match peer.ip() {
            IpAddr::V4(address) => {
                material.push(4);
                material.extend_from_slice(&address.octets());
            }
            IpAddr::V6(address) => {
                material.push(6);
                material.extend_from_slice(&address.octets());
            }
        }
        let tag = hmac::sign(&self.key, &material);
        let mut fingerprint = [0_u8; 32];
        fingerprint.copy_from_slice(tag.as_ref());
        fingerprint
    }
}

/// Invalid Anonymous fingerprint key configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnonymousFingerprintKeyError {
    SecretTooShort,
}

impl std::fmt::Display for AnonymousFingerprintKeyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SecretTooShort => {
                formatter.write_str("Anonymous fingerprint secret must contain at least 32 bytes")
            }
        }
    }
}

impl std::error::Error for AnonymousFingerprintKeyError {}

pub trait AnonymousInvocationBackend: Send + Sync {
    fn invoke(
        &self,
        tenant_id: TenantId,
        object_id: ObjectId,
        subject_fingerprint: [u8; 32],
        idempotency_key: String,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<InvocationId, OwnerApiBackendError>> + Send + '_>>;
}

#[derive(Clone)]
struct AnonymousInvocationState {
    backend: Arc<dyn AnonymousInvocationBackend>,
    fingerprint_key: AnonymousFingerprintKey,
}

impl AnonymousInvocationState {
    fn new(
        backend: impl AnonymousInvocationBackend + 'static,
        fingerprint_key: AnonymousFingerprintKey,
    ) -> Self {
        Self {
            backend: Arc::new(backend),
            fingerprint_key,
        }
    }
}

/// Registers the public Anonymous invocation route.
///
/// Tenant ID is part of the path because Object IDs are tenant-scoped, not
/// globally unique. It is routing context, not an authenticated Principal.
pub fn anonymous_invocation_router(
    backend: impl AnonymousInvocationBackend + 'static,
    fingerprint_key: AnonymousFingerprintKey,
) -> Router {
    Router::new()
        .route(
            "/v1/public/tenants/{tenant_id}/objects/{object_id}/invocations",
            post(invoke_public_object),
        )
        .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(AnonymousInvocationState::new(backend, fingerprint_key))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AnonymousInvokeObjectRequest {
    idempotency_key: String,
    input: serde_json::Value,
}

fn parse_invoke_body(body: &[u8]) -> Result<(String, serde_json::Value), ()> {
    canonicalize_json(body).map_err(|_| ())?;
    let request: AnonymousInvokeObjectRequest = serde_json::from_slice(body).map_err(|_| ())?;
    if request.idempotency_key.trim().is_empty() || request.idempotency_key.len() > 256 {
        return Err(());
    }
    let input_bytes = serde_json::to_vec(&request.input).map_err(|_| ())?;
    let canonical_input = canonicalize_json(&input_bytes).map_err(|_| ())?;
    if canonical_input.len() > MAX_INPUT_BYTES {
        return Err(());
    }
    Ok((request.idempotency_key, request.input))
}

#[derive(Clone, Copy)]
struct DirectPeer(SocketAddr);

impl<S> FromRequestParts<S> for DirectPeer
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(peer)| Self(*peer))
            .ok_or_else(|| {
                api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "edge_context_unavailable",
                    "The request edge context is unavailable.",
                )
            })
    }
}

async fn invoke_public_object(
    State(state): State<AnonymousInvocationState>,
    Path((tenant_id, object_id)): Path<(String, String)>,
    DirectPeer(peer): DirectPeer,
    body: Bytes,
) -> Response {
    let tenant_id = match tenant_id.parse::<TenantId>() {
        Ok(id) => id,
        Err(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The tenant ID is invalid.",
            );
        }
    };
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
    let (idempotency_key, input) = match parse_invoke_body(&body) {
        Ok(request) => request,
        Err(()) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The invocation request body is invalid.",
            );
        }
    };

    let subject_fingerprint = state
        .fingerprint_key
        .fingerprint(peer, tenant_id, object_id);
    match state
        .backend
        .invoke(
            tenant_id,
            object_id,
            subject_fingerprint,
            idempotency_key,
            input,
        )
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
        OwnerApiBackendError::NotFound => api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "The requested public Object was not found.",
        ),
        OwnerApiBackendError::Conflict => api_error(
            StatusCode::CONFLICT,
            "conflict",
            "The invocation conflicts with the current Object or idempotency state.",
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
            if let Some(delay) = retry_after {
                let retry_after = delay.as_secs().max(1).to_string();
                if let Ok(value) = HeaderValue::from_bytes(retry_after.as_bytes()) {
                    response.headers_mut().insert(header::RETRY_AFTER, value);
                }
            }
            response
        }
        OwnerApiBackendError::Unauthorized
        | OwnerApiBackendError::ValidationRejected
        | OwnerApiBackendError::Internal => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "An unexpected internal error occurred.",
        ),
    }
}

fn api_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (status, Json(crate::errors::ApiErrorBody { code, message })).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::{body::Body, extract::ConnectInfo, http::Request};
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone, Default)]
    struct MockBackend {
        fingerprint: Arc<Mutex<Option<[u8; 32]>>>,
    }

    impl AnonymousInvocationBackend for MockBackend {
        fn invoke(
            &self,
            _tenant_id: TenantId,
            _object_id: ObjectId,
            subject_fingerprint: [u8; 32],
            _idempotency_key: String,
            _input: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<InvocationId, OwnerApiBackendError>> + Send + '_>>
        {
            let fingerprint = Arc::clone(&self.fingerprint);
            Box::pin(async move {
                *fingerprint.lock().expect("fingerprint mutex") = Some(subject_fingerprint);
                Ok(InvocationId::new())
            })
        }
    }

    fn test_key() -> AnonymousFingerprintKey {
        AnonymousFingerprintKey::from_secret(&[0x42; 32])
            .expect("32-byte fingerprint key should be accepted")
    }

    #[test]
    fn fingerprint_is_stable_and_scoped_to_peer_tenant_and_object() {
        let key = test_key();
        let tenant = TenantId::new();
        let object = ObjectId::new();
        let peer: SocketAddr = "192.0.2.10:1234".parse().expect("peer address");

        let first = key.fingerprint(peer, tenant, object);
        assert_eq!(first, key.fingerprint(peer, tenant, object));
        assert_ne!(
            first,
            key.fingerprint(
                "192.0.2.11:1234".parse().expect("other peer"),
                tenant,
                object
            )
        );
        assert_ne!(first, key.fingerprint(peer, tenant, ObjectId::new()));
        assert_ne!(first, key.fingerprint(peer, TenantId::new(), object));
    }

    #[tokio::test]
    async fn public_route_uses_direct_peer_and_ignores_forwarded_headers() {
        let backend = MockBackend::default();
        let observed = Arc::clone(&backend.fingerprint);
        let key = test_key();
        let tenant = TenantId::new();
        let object = ObjectId::new();
        let peer: SocketAddr = "192.0.2.10:4567".parse().expect("peer address");
        let expected = key.fingerprint(peer, tenant, object);
        let app = anonymous_invocation_router(backend, key);

        let mut request = Request::builder()
            .method("POST")
            .uri(format!(
                "/v1/public/tenants/{tenant}/objects/{object}/invocations"
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-forwarded-for", "198.51.100.77")
            .body(Body::from(
                r#"{"idempotency_key":"client-key","input":{"message":"hello"}}"#,
            ))
            .expect("request should be valid");
        request.extensions_mut().insert(ConnectInfo(peer));

        let response = app
            .oneshot(request)
            .await
            .expect("router should handle request");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(*observed.lock().expect("fingerprint mutex"), Some(expected));
    }

    #[tokio::test]
    async fn public_route_rejects_caller_id_in_transport_body() {
        let app = anonymous_invocation_router(MockBackend::default(), test_key());
        let tenant = TenantId::new();
        let object = ObjectId::new();
        let peer: SocketAddr = "192.0.2.10:4567".parse().expect("peer address");
        let mut request = Request::builder()
            .method("POST")
            .uri(format!(
                "/v1/public/tenants/{tenant}/objects/{object}/invocations"
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"caller_id":"20000000-0000-0000-0000-000000000001","idempotency_key":"key","input":{}}"#,
            ))
            .expect("request should be valid");
        request.extensions_mut().insert(ConnectInfo(peer));

        let response = app
            .oneshot(request)
            .await
            .expect("router should handle request");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn public_route_fails_closed_without_direct_peer_context() {
        let app = anonymous_invocation_router(MockBackend::default(), test_key());
        let tenant = TenantId::new();
        let object = ObjectId::new();
        let request = Request::builder()
            .method("POST")
            .uri(format!(
                "/v1/public/tenants/{tenant}/objects/{object}/invocations"
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"idempotency_key":"client-key","input":{}}"#))
            .expect("request should be valid");

        let response = app
            .oneshot(request)
            .await
            .expect("router should handle request");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
