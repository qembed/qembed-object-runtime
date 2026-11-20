//! Tower-based test helpers shared by PostgreSQL API integration tests.
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
