//! Tower-based test helpers shared by PostgreSQL API integration tests.

use qembed_store::{ContextIssuer, ContextIssuerConfig, PgContext, RingEd25519SigningKey};
use sqlx::PgPool;
use uuid::Uuid;

/// Creates a test-only signer and registers its public key through the dedicated
/// PostgreSQL key-control-plane identity.
#[allow(dead_code)]
pub async fn signed_context(pool: &PgPool) -> PgContext {
    let deployment_id = "rt050-api-integration-test";
    let database_oid: String =
        sqlx::query_scalar("SELECT oid::text FROM pg_database WHERE datname = current_database()")
            .fetch_one(pool)
            .await
            .expect("database OID should be readable");
    let audience =
        format!("urn:qembed:postgres:deployment:{deployment_id}:database:{database_oid}");
    let issuer_name = "qembed-runtime";
    let key_id = format!("rt050-api-test-{}", Uuid::new_v4());
    let signing_key = RingEd25519SigningKey::from_seed(key_id.clone(), &[43_u8; 32])
        .expect("the fixed API-test seed should create a valid Ed25519 signer");
    let public_key = signing_key.public_key();

    let mut control_plane = pool
        .acquire()
        .await
        .expect("key control-plane connection should be acquired");
    sqlx::query("SET SESSION AUTHORIZATION qembed_key_control_plane")
        .execute(&mut *control_plane)
        .await
        .expect("test harness should assume the dedicated key-control-plane identity");

    sqlx::query("SELECT qembed_crypto.stage_verification_key($1, $2, $3, $4, $5, $6, $7, $8)")
        .bind(&key_id)
        .bind(public_key.to_vec())
        .bind(issuer_name)
        .bind(&audience)
        .bind(deployment_id)
        .bind("operator:rt050-api-test")
        .bind(Uuid::new_v4())
        .bind("register API integration-test signer")
        .execute(&mut *control_plane)
        .await
        .expect("control plane should stage the public verification key");

    let _: String = sqlx::query_scalar(
        "SELECT qembed_crypto.activate_verification_key($1, $2, $3, $4, $5)::text",
    )
    .bind(&key_id)
    .bind("operator:rt050-api-test")
    .bind("approver:rt050-api-test")
    .bind(Uuid::new_v4())
    .bind("activate API integration-test signer")
    .fetch_one(&mut *control_plane)
    .await
    .expect("a distinct approver should activate the API integration-test key");

    sqlx::query("RESET SESSION AUTHORIZATION")
        .execute(&mut *control_plane)
        .await
        .expect("control-plane identity must be reset before pool reuse");
    drop(control_plane);

    let config =
        ContextIssuerConfig::with_default_lifetime(issuer_name, audience, deployment_id, key_id)
            .expect("API test issuer configuration should be valid");
    let issuer = ContextIssuer::new(config, signing_key)
        .expect("API test issuer configuration and signing key should match");
    PgContext::new(pool.clone(), issuer)
}

pub mod test {
    use axum::{
        body::{Body, to_bytes},
        http::{HeaderName, HeaderValue, Method, Request, Response, Uri},
    };
    use std::fmt::Debug;
    use tower::ServiceExt;

    pub async fn init_service(router: axum::Router) -> axum::Router {
        router
    }

    pub async fn call_service(app: &axum::Router, request: Request<Body>) -> Response<Body> {
        app.clone()
            .oneshot(request)
            .await
            .expect("router service should not fail")
    }

    pub async fn read_body(response: Response<Body>) -> axum::body::Bytes {
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable")
    }

    pub struct TestRequest {
        method: Method,
        uri: Uri,
        headers: axum::http::HeaderMap,
        body: Body,
    }

    impl TestRequest {
        pub fn post() -> Self {
            Self::new(Method::POST)
        }

        #[allow(dead_code)]
        pub fn get() -> Self {
            Self::new(Method::GET)
        }

        fn new(method: Method) -> Self {
            Self {
                method,
                uri: "/".parse().unwrap(),
                headers: Default::default(),
                body: Body::empty(),
            }
        }

        pub fn uri(mut self, uri: &str) -> Self {
            self.uri = uri.parse().expect("valid request URI");
            self
        }

        pub fn insert_header<K, V>(mut self, pair: (K, V)) -> Self
        where
            K: TryInto<HeaderName>,
            K::Error: Debug,
            V: TryInto<HeaderValue>,
            V::Error: Debug,
        {
            self.headers.insert(
                pair.0.try_into().expect("valid header name"),
                pair.1.try_into().expect("valid header value"),
            );
            self
        }

        #[allow(dead_code)]
        pub fn set_payload(mut self, payload: impl Into<Body>) -> Self {
            self.body = payload.into();
            self
        }

        #[allow(dead_code)]
        pub fn set_json<T: serde::Serialize>(mut self, value: T) -> Self {
            self.headers.insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            self.body = Body::from(serde_json::to_vec(&value).expect("request JSON serializes"));
            self
        }

        #[allow(clippy::wrong_self_convention)]
        pub fn to_request(self) -> Request<Body> {
            let mut request = Request::builder()
                .method(self.method)
                .uri(self.uri)
                .body(self.body)
                .expect("request builds");
            *request.headers_mut() = self.headers;
            request
        }
    }
}
