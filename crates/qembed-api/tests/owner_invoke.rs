use axum::http::StatusCode;
mod support;
use qembed_domain::ids::{
    CallerId, InvocationId, ObjectId, ObjectVersionId, PrincipalId, TenantId,
};
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
    let other_tenant_id = TenantId::new();
    let other_principal_id = PrincipalId::new();
    let other_token = format!("owner-invoke-other-{}", PrincipalId::new());
    let principal_id = PrincipalId::new();
    let caller_id = CallerId::new();
    let token = format!("owner-invoke-{}", PrincipalId::new());
    sqlx::query("INSERT INTO tenants (id) VALUES ($1::uuid), ($2::uuid)")
        .bind(tenant_id.to_string())
        .bind(other_tenant_id.to_string())
        .execute(&pool)
        .await
        .expect("tenant fixtures should be inserted");
    sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) VALUES ($1::uuid, $2::uuid, 'tenant_owner')",
    )
    .bind(other_tenant_id.to_string())
    .bind(other_principal_id.to_string())
    .execute(&pool)
    .await
    .expect("second tenant Owner fixture should be inserted");
    sqlx::query(
        "INSERT INTO owner_credentials (tenant_id, caller_id, token_hash) VALUES ($1::uuid, $2::uuid, $3)",
    )
    .bind(other_tenant_id.to_string())
    .bind(other_principal_id.to_string())
    .bind(Sha256::digest(other_token.as_bytes()).to_vec())
    .execute(&pool)
    .await
    .expect("second tenant Owner credential should be inserted");
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

    let context = support::signed_context(&pool).await;
    let app = test::init_service(configure_owner_routes(pool.clone(), context)).await;
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
    let first_snapshot_id = version_json["snapshot_id"]
        .as_str()
        .expect("snapshot_id")
        .to_owned();

    let publish_uri = format!("/owner/objects/{object_id}/versions/{version_id}/publish");
    let publish_response = test::call_service(&app, headers(&publish_uri).to_request()).await;
    assert_eq!(publish_response.status(), StatusCode::OK);

    // Budget policy is provisioned independently from object creation. The
    // invocation path deliberately fails closed when this configuration is absent.
    sqlx::query(
        "INSERT INTO object_budgets (tenant_id, object_id, daily_token_limit, daily_cost_limit_micros, per_caller_daily_token_limit, max_concurrent_invocations, max_llm_calls, max_tool_calls, max_tokens_per_invocation, max_duration_ms) VALUES ($1::uuid, $2::uuid, 100000, 0, 100000, 10, 10, 0, 4096, 60000)",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(&pool)
    .await
    .expect("object budget policy should be provisioned for invocation");

    let invoke_uri = format!("/owner/objects/{object_id}/invocations");
    let undelegated = test::call_service(
        &app,
        headers(&invoke_uri)
            .set_json(serde_json::json!({
                "caller_id": CallerId::new(),
                "idempotency_key": "undelegated-caller",
                "input": { "question": "must be denied" }
            }))
            .to_request(),
    )
    .await;
    assert_eq!(undelegated.status(), StatusCode::NOT_FOUND);

    let request = serde_json::json!({
        "caller_id": caller_id,
        "idempotency_key": "owner-invoke-key-1",
        "input": { "question": "RUST091_RAW_INPUT_SENTINEL_DO_NOT_EMIT", "sequence": 1 }
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
    let first_response_text = first_json.to_string();
    assert!(!first_response_text.contains("RUST091_RAW_INPUT_SENTINEL_DO_NOT_EMIT"));
    assert!(!first_response_text.contains(&token));
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

    // Owner inspection exposes only the minimized projection and keeps
    // missing and cross-tenant invocation IDs indistinguishable.
    let inspected = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/owner/invocations/{invocation_id}"))
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((axum::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(inspected.status(), StatusCode::OK);
    let inspected_json: serde_json::Value =
        serde_json::from_slice(&test::read_body(inspected).await).expect("inspection JSON");
    assert_eq!(inspected_json["invocation_id"], invocation_id);
    assert_eq!(inspected_json["status"], "queued");
    let inspected_text = inspected_json.to_string();
    assert!(!inspected_text.contains("RUST091_RAW_INPUT_SENTINEL_DO_NOT_EMIT"));
    assert!(!inspected_text.contains(&token));
    let inspected_fields = inspected_json.as_object().expect("inspection object");
    assert_eq!(inspected_fields.len(), 5);
    for forbidden in [
        "tenant_id",
        "caller_id",
        "snapshot_id",
        "trace_id",
        "lease_owner",
        "fencing_token",
        "input",
        "provider",
    ] {
        assert!(
            !inspected_fields.contains_key(forbidden),
            "leaked {forbidden}"
        );
    }

    let missing_id = InvocationId::new();
    let missing = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/owner/invocations/{missing_id}"))
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((axum::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let cross_tenant = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/owner/invocations/{invocation_id}"))
            .insert_header(("X-Tenant-ID", other_tenant_id.to_string()))
            .insert_header((
                axum::http::header::AUTHORIZATION,
                format!("Bearer {other_token}"),
            ))
            .to_request(),
    )
    .await;
    assert_eq!(cross_tenant.status(), StatusCode::NOT_FOUND);
    let missing_body = test::read_body(missing).await;
    let cross_tenant_body = test::read_body(cross_tenant).await;
    assert_eq!(missing_body, cross_tenant_body);

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

    // The durable invocation event and its outbox projection carry routing
    // metadata only. Raw request input and bearer credentials stay out of the
    // event transport boundary.
    let event_payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM events WHERE tenant_id=$1::uuid AND aggregate_id=$2::uuid AND event_type='invocation.received'",
    )
    .bind(tenant_id.to_string())
    .bind(&invocation_id)
    .fetch_one(&pool)
    .await
    .expect("invocation event payload should be queryable");
    let outbox_payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_messages WHERE tenant_id=$1::uuid AND payload->>'invocation_id'=$2",
    )
    .bind(tenant_id.to_string())
    .bind(&invocation_id)
    .fetch_one(&pool)
    .await
    .expect("invocation outbox payload should be queryable");
    let expected_event_fields = [
        "caller_id",
        "caller_kind",
        "invocation_id",
        "invocation_pool",
        "knowledge_revision",
        "object_id",
        "snapshot_id",
        "type",
    ];
    for payload in [&event_payload, &outbox_payload] {
        let object = payload.as_object().expect("event payload is an object");
        let mut actual_fields = object.keys().map(String::as_str).collect::<Vec<_>>();
        actual_fields.sort_unstable();
        assert_eq!(actual_fields, expected_event_fields);
        let serialized = payload.to_string();
        assert!(!serialized.contains("RUST091_RAW_INPUT_SENTINEL_DO_NOT_EMIT"));
        assert!(!serialized.contains(&token));
        assert!(!serialized.contains("idempotency_key"));
        assert!(!serialized.contains("arguments"));
        assert!(!serialized.contains("payload_ciphertext"));
    }

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

    // Race a replacement publication against another invocation start. The
    // Store row-lock protocol permits either committed Snapshot, never a Draft
    // or an unrelated Snapshot.
    let replacement_uri = format!("/owner/objects/{object_id}/versions");
    let replacement = test::call_service(
        &app,
        headers(&replacement_uri)
            .set_json(serde_json::json!({
                "schema_version": 1,
                "config": {
                    "limits": { "max_tokens_per_invocation": 2048 },
                    "revision": "concurrent-replacement"
                }
            }))
            .to_request(),
    )
    .await;
    assert_eq!(replacement.status(), StatusCode::CREATED);
    let replacement_json: serde_json::Value =
        serde_json::from_slice(&test::read_body(replacement).await).expect("replacement JSON");
    let replacement_version_id = replacement_json["object_version_id"]
        .as_str()
        .expect("replacement version ID")
        .parse::<ObjectVersionId>()
        .expect("valid replacement version ID");
    let replacement_snapshot_id = replacement_json["snapshot_id"]
        .as_str()
        .expect("replacement snapshot ID")
        .to_owned();

    let publish_uri =
        format!("/owner/objects/{object_id}/versions/{replacement_version_id}/publish");
    let race_invoke_uri = format!("/owner/objects/{object_id}/invocations");
    let publish_request = headers(&publish_uri).to_request();
    let invoke_request = headers(&race_invoke_uri)
        .set_json(serde_json::json!({
            "caller_id": caller_id,
            "idempotency_key": "owner-invoke-publish-race",
            "input": { "question": "race publication and invocation" }
        }))
        .to_request();
    let (publish_race, invoke_race) = tokio::join!(
        test::call_service(&app, publish_request),
        test::call_service(&app, invoke_request)
    );
    assert_eq!(publish_race.status(), StatusCode::OK);
    assert_eq!(invoke_race.status(), StatusCode::ACCEPTED);
    let race_invocation_json: serde_json::Value =
        serde_json::from_slice(&test::read_body(invoke_race).await)
            .expect("race invocation response JSON");
    let race_invocation_id = race_invocation_json["invocation_id"]
        .as_str()
        .expect("race invocation ID");
    let pinned_snapshot: String = sqlx::query_scalar(
        "SELECT snapshot_id::text FROM object_invocations WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND id=$3::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(race_invocation_id)
    .fetch_one(&pool)
    .await
    .expect("racing invocation should persist");
    assert!(
        pinned_snapshot == first_snapshot_id || pinned_snapshot == replacement_snapshot_id,
        "invocation must pin either the prior published Snapshot or the replacement Snapshot"
    );
    let pinned_version_status: String = sqlx::query_scalar(
        "SELECT ov.status::text FROM snapshots s JOIN object_versions ov USING (tenant_id, object_id, object_version_id) WHERE s.tenant_id=$1::uuid AND s.object_id=$2::uuid AND s.snapshot_id=$3::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(&pinned_snapshot)
    .fetch_one(&pool)
    .await
    .expect("pinned snapshot must resolve to its version");
    assert!(
        pinned_version_status == "published" || pinned_version_status == "superseded",
        "invocation must not pin a Draft Snapshot"
    );

    drop(app);
    pool.close().await;
}
