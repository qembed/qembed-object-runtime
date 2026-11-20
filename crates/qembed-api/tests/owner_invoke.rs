use axum::http::StatusCode;
mod support;
use qembed_domain::ids::{CallerId, ObjectId, ObjectVersionId, PrincipalId, TenantId};
use qembed_store::{PgPoolConfig, connect_pool, run_migrations};
use sha2::{Digest, Sha256};
use support::test;

use qembed_api::composition::configure_owner_routes;

#[tokio::test]
async fn owner_invoke_pins_published_snapshot_and_replays_idempotently() {
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
    let caller_id = CallerId::new();
    let token = format!("owner-invoke-{}", PrincipalId::new());
    sqlx::query("INSERT INTO tenants (id) VALUES ($1::uuid)")
        .bind(tenant_id.to_string())
        .execute(&pool)
        .await
        .expect("tenant fixture should be inserted");
    sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) VALUES ($1::uuid, $2::uuid, 'tenant_owner')",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .execute(&pool)
    .await
    .expect("owner Principal fixture should be inserted");
    sqlx::query(
        "INSERT INTO owner_credentials (tenant_id, caller_id, token_hash) VALUES ($1::uuid, $2::uuid, $3)",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .bind(Sha256::digest(token.as_bytes()).to_vec())
    .execute(&pool)
    .await
    .expect("owner credential fixture should be inserted");
    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind) VALUES ($1::uuid, $2::uuid, $3, 'Service')",
    )
    .bind(caller_id.to_string())
    .bind(tenant_id.to_string())
    .bind(caller_id.into_uuid().as_bytes().to_vec())
    .execute(&pool)
    .await
    .expect("Caller fixture should be inserted");
    sqlx::query(
        "INSERT INTO principal_caller_delegations (delegation_id, tenant_id, principal_id, caller_id) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid)",
    )
    .bind(PrincipalId::new().to_string())
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .bind(caller_id.to_string())
    .execute(&pool)
    .await
    .expect("explicit Principal-to-Caller delegation should be inserted");

    let app = test::init_service(configure_owner_routes(pool.clone())).await;
    let headers = |uri: &str| {
        test::TestRequest::post()
            .uri(uri)
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((axum::http::header::AUTHORIZATION, format!("Bearer {token}")))
    };

    let object_response = test::call_service(&app, headers("/owner/objects").to_request()).await;
    assert_eq!(object_response.status(), StatusCode::CREATED);
    let object_json: serde_json::Value =
        serde_json::from_slice(&test::read_body(object_response).await).expect("object JSON");
    let object_id = object_json["object_id"]
        .as_str()
        .expect("object_id")
        .parse::<ObjectId>()
        .expect("valid object ID");

    let version_uri = format!("/owner/objects/{object_id}/versions");
    let version_response = test::call_service(
        &app,
        headers(&version_uri)
            .set_json(serde_json::json!({
                "schema_version": 1,
                "config": { "limits": { "max_tokens_per_invocation": 4096 } }
            }))
            .to_request(),
    )
    .await;
    assert_eq!(version_response.status(), StatusCode::CREATED);
    let version_json: serde_json::Value =
        serde_json::from_slice(&test::read_body(version_response).await).expect("version JSON");
    let version_id = version_json["object_version_id"]
        .as_str()
        .expect("object_version_id")
        .parse::<ObjectVersionId>()
        .expect("valid version ID");

    let publish_uri = format!("/owner/objects/{object_id}/versions/{version_id}/publish");
    let publish_response = test::call_service(&app, headers(&publish_uri).to_request()).await;
    assert_eq!(publish_response.status(), StatusCode::OK);

    let invoke_uri = format!("/owner/objects/{object_id}/invocations");
    let request = serde_json::json!({
        "caller_id": caller_id,
        "idempotency_key": "owner-invoke-key-1",
        "input": { "question": "hello", "sequence": 1 }
    });
    let first = test::call_service(
        &app,
        headers(&invoke_uri).set_json(request.clone()).to_request(),
    )
    .await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let first_json: serde_json::Value =
        serde_json::from_slice(&test::read_body(first).await).expect("accepted response JSON");
    assert_eq!(first_json["status"], "received");
    let invocation_id = first_json["invocation_id"]
        .as_str()
        .expect("invocation_id")
        .to_owned();

    let replay = test::call_service(
        &app,
        headers(&invoke_uri).set_json(request.clone()).to_request(),
    )
    .await;
    assert_eq!(replay.status(), StatusCode::ACCEPTED);
    let replay_json: serde_json::Value =
        serde_json::from_slice(&test::read_body(replay).await).expect("replay response JSON");
    assert_eq!(replay_json["invocation_id"], invocation_id);

    let mismatch = test::call_service(
        &app,
        headers(&invoke_uri)
            .set_json(serde_json::json!({
                "caller_id": caller_id,
                "idempotency_key": "owner-invoke-key-1",
                "input": { "question": "different" }
            }))
            .to_request(),
    )
    .await;
    assert_eq!(mismatch.status(), StatusCode::CONFLICT);

    let persisted: (String, String, String, String, String) = sqlx::query_as(
        "SELECT caller_id::text, snapshot_id::text, pool, status, trace_id::text FROM object_invocations WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND id=$3::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(&invocation_id)
    .fetch_one(&pool)
    .await
    .expect("invocation should be durably persisted");
    assert_eq!(persisted.0, caller_id.to_string());
    assert_eq!(persisted.2, "authenticated");
    assert_eq!(persisted.3, "Received");
    assert_eq!(persisted.4.len(), 36);

    let counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM object_invocations WHERE tenant_id=$1::uuid AND object_id=$2::uuid), (SELECT count(*) FROM invocation_idempotency WHERE tenant_id=$1::uuid AND object_id=$2::uuid), (SELECT count(*) FROM interaction_messages WHERE tenant_id=$1::uuid AND object_id=$2::uuid), (SELECT count(*) FROM outbox_messages WHERE tenant_id=$1::uuid AND payload->>'invocation_id'=$3)",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(&invocation_id)
    .fetch_one(&pool)
    .await
    .expect("invocation side-effect counts should be queryable");
    assert_eq!(counts, (1, 1, 1, 1));

    let retention: (i64, i64) = sqlx::query_as(
        "SELECT round(extract(epoch FROM (expires_at - created_at)))::bigint, (SELECT round(extract(epoch FROM (expires_at - created_at)))::bigint FROM interaction_messages WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND invocation_id=$3::uuid AND role='user') FROM invocation_idempotency WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND invocation_id=$3::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(&invocation_id)
    .fetch_one(&pool)
    .await
    .expect("retention deadlines should be queryable");
    assert!((86_390..=86_410).contains(&retention.0));
    assert!((2_591_990..=2_592_010).contains(&retention.1));

    drop(app);
    pool.close().await;
}
