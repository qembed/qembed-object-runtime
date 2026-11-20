use axum::http::StatusCode;
mod support;
use qembed_domain::ids::{ObjectId, PrincipalId, TenantId};
use qembed_store::{PgPoolConfig, connect_pool, run_migrations};
use sha2::{Digest, Sha256};
use support::test;

use qembed_api::composition::configure_owner_routes;

#[tokio::test]
async fn owner_create_object_authenticates_and_persists_a_server_generated_draft() {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by the PostgreSQL integration-test job");
    let pool = connect_pool(&database_url, PgPoolConfig::default())
        .await
        .expect("PostgreSQL should accept connections");
    run_migrations(&pool)
        .await
        .expect("the test database migrations should be applied");

    let tenant_id = TenantId::new();
    let principal_id = PrincipalId::new();
    let token = format!("owner-create-{}", PrincipalId::new());
    let token_hash = Sha256::digest(token.as_bytes()).to_vec();

    sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) VALUES ($1::uuid, $2::uuid, 'tenant_owner')",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .execute(&pool)
    .await
    .expect("owner principal fixture should be inserted");
    sqlx::query(
        "INSERT INTO owner_credentials (tenant_id, caller_id, token_hash) VALUES ($1::uuid, $2::uuid, $3)",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .bind(token_hash)
    .execute(&pool)
    .await
    .expect("owner credential fixture should be inserted");

    let app = test::init_service(configure_owner_routes(pool.clone())).await;

    let oversized_body = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/owner/objects")
            .set_payload(vec![0_u8; 256 * 1024 + 1])
            .to_request(),
    )
    .await;
    assert_eq!(oversized_body.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let missing_tenant = test::call_service(
        &app,
        test::TestRequest::post().uri("/owner/objects").to_request(),
    )
    .await;
    assert_eq!(missing_tenant.status(), StatusCode::BAD_REQUEST);

    let missing_auth = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/owner/objects")
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .to_request(),
    )
    .await;
    assert_eq!(missing_auth.status(), StatusCode::UNAUTHORIZED);

    let wrong_tenant = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/owner/objects")
            .insert_header(("X-Tenant-ID", TenantId::new().to_string()))
            .insert_header((axum::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(wrong_tenant.status(), StatusCode::UNAUTHORIZED);

    let invalid_body = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/owner/objects")
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((axum::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .set_json(serde_json::json!({ "object_id": ObjectId::new() }))
            .to_request(),
    )
    .await;
    assert_eq!(invalid_body.status(), StatusCode::BAD_REQUEST);

    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/owner/objects")
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((axum::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = test::read_body(response).await;
    let json: serde_json::Value = serde_json::from_slice(&body).expect("response is JSON");
    let object_id = json["object_id"]
        .as_str()
        .expect("response contains object_id")
        .parse::<ObjectId>()
        .expect("server-generated object ID is a UUID");
    assert_eq!(json["state"], "draft");
    assert_eq!(json.as_object().expect("response is an object").len(), 2);

    let persisted_state: String = sqlx::query_scalar(
        "SELECT state FROM objects WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("created object should be persisted");
    assert_eq!(persisted_state, "draft");

    let other_tenant_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM objects WHERE tenant_id <> $1::uuid AND object_id = $2::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("cross-tenant absence should be queryable");
    assert_eq!(other_tenant_count, 0);

    drop(app);
    pool.close().await;
}
