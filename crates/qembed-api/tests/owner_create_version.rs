use actix_web::{App, http::StatusCode, test};
use qembed_domain::ids::{ObjectId, PrincipalId, TenantId};
use qembed_store::{PgPoolConfig, connect_pool, run_migrations};
use sha2::{Digest, Sha256};

use qembed_api::composition::configure_owner_routes;

async fn insert_owner(pool: &sqlx::PgPool, tenant_id: TenantId, token: &str) {
    let principal_id = PrincipalId::new();
    sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) VALUES ($1::uuid, $2::uuid, 'tenant_owner')",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .execute(pool)
    .await
    .expect("owner principal fixture should be inserted");

    sqlx::query(
        "INSERT INTO owner_credentials (tenant_id, caller_id, token_hash) VALUES ($1::uuid, $2::uuid, $3)",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .bind(Sha256::digest(token.as_bytes()).to_vec())
    .execute(pool)
    .await
    .expect("owner credential fixture should be inserted");
}

#[actix_web::test]
async fn owner_create_version_persists_canonical_draft_and_reuses_identical_content() {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by the PostgreSQL integration-test job");
    let pool = connect_pool(&database_url, PgPoolConfig::default())
        .await
        .expect("PostgreSQL should accept connections");
    run_migrations(&pool)
        .await
        .expect("the test database migrations should be applied");

    let tenant_id = TenantId::new();
    let other_tenant_id = TenantId::new();
    let token = format!("owner-version-{}", PrincipalId::new());
    let other_token = format!("owner-version-{}", PrincipalId::new());
    insert_owner(&pool, tenant_id, &token).await;
    insert_owner(&pool, other_tenant_id, &other_token).await;

    let app = test::init_service(
        App::new().configure(|config| configure_owner_routes(config, pool.clone())),
    )
    .await;

    let object_response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/owner/objects")
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(object_response.status(), StatusCode::CREATED);
    let object_json: serde_json::Value =
        serde_json::from_slice(&test::read_body(object_response).await)
            .expect("object response is JSON");
    let object_id = object_json["object_id"]
        .as_str()
        .expect("object response contains ID")
        .parse::<ObjectId>()
        .expect("object ID is valid");

    // The missing max_tokens_per_invocation field is intentionally not checked
    // here. Draft creation stores a valid JSON snapshot; publication validates it.
    let first = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(format!("/owner/objects/{object_id}/versions"))
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .insert_header((actix_web::http::header::CONTENT_TYPE, "application/json"))
            .set_payload(r#"{"schema_version":1,"config":{"limits":{},"unvalidated":true}}"#)
            .to_request(),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_json: serde_json::Value =
        serde_json::from_slice(&test::read_body(first).await).expect("version response is JSON");
    assert_eq!(first_json["outcome"], "created");
    let version_id = first_json["object_version_id"]
        .as_str()
        .expect("version ID is returned")
        .to_owned();
    let snapshot_id = first_json["snapshot_id"]
        .as_str()
        .expect("snapshot ID is returned")
        .to_owned();

    let persisted: (i64, String, i32, Vec<u8>) = sqlx::query_as(
        "SELECT ov.version_number, ov.status, s.schema_version, s.canonical_json FROM object_versions ov JOIN snapshots s USING (tenant_id, object_id, object_version_id) WHERE ov.tenant_id = $1::uuid AND ov.object_id = $2::uuid AND ov.object_version_id = $3::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(&version_id)
    .fetch_one(&pool)
    .await
    .expect("version and snapshot should be persisted together");
    assert_eq!(persisted.0, 1);
    assert_eq!(persisted.1, "draft");
    assert_eq!(persisted.2, 1);
    assert_eq!(persisted.3.as_slice(), br#"{"limits":{},"unvalidated":true}"#);

    let duplicate = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(format!("/owner/objects/{object_id}/versions"))
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .insert_header((actix_web::http::header::CONTENT_TYPE, "application/json"))
            .set_payload(r#"{"config":{"unvalidated":true,"limits":{}},"schema_version":1}"#)
            .to_request(),
    )
    .await;
    assert_eq!(duplicate.status(), StatusCode::OK);
    let duplicate_json: serde_json::Value =
        serde_json::from_slice(&test::read_body(duplicate).await).expect("dedup response is JSON");
    assert_eq!(duplicate_json["outcome"], "identical_content");
    assert_eq!(duplicate_json["object_version_id"], version_id);
    assert_eq!(duplicate_json["snapshot_id"], snapshot_id);

    let malformed = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(format!("/owner/objects/{object_id}/versions"))
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .set_payload(r#"{"schema_version":1,"config":{"x":1,"x":2}}"#)
            .to_request(),
    )
    .await;
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);

    let cross_tenant = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(format!("/owner/objects/{object_id}/versions"))
            .insert_header(("X-Tenant-ID", other_tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {other_token}")))
            .insert_header((actix_web::http::header::CONTENT_TYPE, "application/json"))
            .set_payload(r#"{"schema_version":1,"config":{}}"#)
            .to_request(),
    )
    .await;
    assert_eq!(cross_tenant.status(), StatusCode::NOT_FOUND);

    let mismatch = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(format!("/owner/objects/{object_id}/versions"))
            .insert_header(("X-Tenant-ID", other_tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .insert_header((actix_web::http::header::CONTENT_TYPE, "application/json"))
            .set_payload(r#"{"schema_version":1,"config":{}}"#)
            .to_request(),
    )
    .await;
    assert_eq!(mismatch.status(), StatusCode::UNAUTHORIZED);

    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid), (SELECT count(*) FROM snapshots WHERE tenant_id = $1::uuid AND object_id = $2::uuid)",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("row counts should be queryable");
    assert_eq!(counts, (1, 1));

    drop(app);
    pool.close().await;
}
