use axum::http::StatusCode;
mod support;

use qembed_domain::{
    auth::authenticate_owner,
    chat::{ChatFinishReason, ChatMessage, ChatRequest, ChatResponse, ChatRole, TokenUsage},
    ids::{CallerId, InvocationId, ObjectId, ObjectVersionId, PrincipalId, TenantId},
};
use qembed_llm::mock::MockChatProvider;
use qembed_runtime::invocation_worker::{
    InvocationWorker, InvocationWorkerConfig, InvocationWorkerOutcome,
};
use qembed_store::{
    PgPoolConfig,
    auth::{OwnerAuthenticator, OwnerCredentials},
    connect_pool,
    invocation_worker::PgExecutionStore,
    run_migrations,
};
use sha2::{Digest, Sha256};
use support::test;

use qembed_api::composition::configure_owner_routes;

const OWNER_TOKEN_PREFIX: &str = "SECRET_SENTINEL_DO_NOT_LOG_";
const MOCK_MODEL: &str = "test-chat";
const USER_INPUT: &str = r#"{"question":"hello from the walking skeleton","sequence":1}"#;
const MOCK_OUTPUT: &str = "deterministic walking-skeleton answer";

fn owner_request(uri: &str, tenant_id: TenantId, token: &str) -> test::TestRequest {
    test::TestRequest::post()
        .uri(uri)
        .insert_header(("X-Tenant-ID", tenant_id.to_string()))
        .insert_header((axum::http::header::AUTHORIZATION, format!("Bearer {token}")))
}

async fn seed_owner(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    principal_id: PrincipalId,
    token: &str,
) {
    sqlx::query("INSERT INTO tenants (id) VALUES ($1::uuid)")
        .bind(tenant_id.to_string())
        .execute(pool)
        .await
        .expect("tenant fixture should be inserted");
    sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) VALUES ($1::uuid, $2::uuid, 'tenant_owner')",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .execute(pool)
    .await
    .expect("Owner Principal fixture should be inserted");
    sqlx::query(
        "INSERT INTO owner_credentials (tenant_id, caller_id, token_hash) VALUES ($1::uuid, $2::uuid, $3)",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .bind(Sha256::digest(token.as_bytes()).to_vec())
    .execute(pool)
    .await
    .expect("Owner credential fixture should be inserted");
}

#[tokio::test]
async fn walking_skeleton_survives_process_restart_and_returns_tenant_scoped_result() {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by the PostgreSQL integration-test job");

    let tenant_id = TenantId::new();
    let principal_id = PrincipalId::new();
    let owner_token = format!("{OWNER_TOKEN_PREFIX}{}", PrincipalId::new());
    let caller_id = CallerId::new();

    let other_tenant_id = TenantId::new();
    let other_principal_id = PrincipalId::new();
    let other_owner_token = format!("{OWNER_TOKEN_PREFIX}other-{}", PrincipalId::new());

    // Process 1: provision the persisted identities and exercise the Owner HTTP API.
    let first_pool = connect_pool(&database_url, PgPoolConfig::default())
        .await
        .expect("PostgreSQL should accept connections");
    run_migrations(&first_pool)
        .await
        .expect("the test database migrations should be applied");

    seed_owner(&first_pool, tenant_id, principal_id, &owner_token).await;
    seed_owner(
        &first_pool,
        other_tenant_id,
        other_principal_id,
        &other_owner_token,
    )
    .await;

    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind) VALUES ($1::uuid, $2::uuid, $3, 'Service')",
    )
    .bind(caller_id.to_string())
    .bind(tenant_id.to_string())
    .bind(caller_id.into_uuid().as_bytes().to_vec())
    .execute(&first_pool)
    .await
    .expect("Caller fixture should be inserted");
    sqlx::query(
        "INSERT INTO principal_caller_delegations (delegation_id, tenant_id, principal_id, caller_id) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid)",
    )
    .bind(PrincipalId::new().to_string())
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .bind(caller_id.to_string())
    .execute(&first_pool)
    .await
    .expect("explicit Principal-to-Caller delegation should be inserted");

    let first_app = test::init_service(configure_owner_routes(first_pool.clone())).await;

    let object_response = test::call_service(
        &first_app,
        owner_request("/owner/objects", tenant_id, &owner_token).to_request(),
    )
    .await;
    assert_eq!(object_response.status(), StatusCode::CREATED);
    let object_json: serde_json::Value =
        serde_json::from_slice(&test::read_body(object_response).await).expect("object JSON");
    let object_id = object_json["object_id"]
        .as_str()
        .expect("object_id")
        .parse::<ObjectId>()
        .expect("valid Object ID");

    let version_uri = format!("/owner/objects/{object_id}/versions");
    let version_response = test::call_service(
        &first_app,
        owner_request(&version_uri, tenant_id, &owner_token)
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
        .expect("valid ObjectVersion ID");

    let publish_uri = format!("/owner/objects/{object_id}/versions/{version_id}/publish");
    let publish_response = test::call_service(
        &first_app,
        owner_request(&publish_uri, tenant_id, &owner_token).to_request(),
    )
    .await;
    assert_eq!(publish_response.status(), StatusCode::OK);

    // Caller delegation and budget are explicit provisioning prerequisites,
    // deliberately outside the Owner API's current endpoint set.
    sqlx::query(
        "INSERT INTO object_budgets (tenant_id, object_id, daily_token_limit, daily_cost_limit_micros, per_caller_daily_token_limit, max_concurrent_invocations, max_llm_calls, max_tool_calls, max_tokens_per_invocation, max_duration_ms) VALUES ($1::uuid, $2::uuid, 100000, 0, 100000, 10, 10, 0, 4096, 60000)",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(&first_pool)
    .await
    .expect("Object budget should be provisioned");

    let invoke_uri = format!("/owner/objects/{object_id}/invocations");
    let invoke_body = serde_json::json!({
        "caller_id": caller_id,
        "idempotency_key": "walking-skeleton-e2e-key",
        "input": {
            "question": "hello from the walking skeleton",
            "sequence": 1
        }
    });
    let accepted = test::call_service(
        &first_app,
        owner_request(&invoke_uri, tenant_id, &owner_token)
            .set_json(invoke_body.clone())
            .to_request(),
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    let accepted_body = test::read_body(accepted).await;
    assert!(
        !String::from_utf8_lossy(&accepted_body).contains(&owner_token),
        "Owner credential must never appear in an API response"
    );
    let accepted_json: serde_json::Value =
        serde_json::from_slice(&accepted_body).expect("accepted JSON");
    assert_eq!(accepted_json["status"], "received");
    let invocation_id = accepted_json["invocation_id"]
        .as_str()
        .expect("invocation_id")
        .parse::<InvocationId>()
        .expect("valid Invocation ID");

    // A duplicate request reuses the same durable invocation and does not
    // create a second message, outbox item, reservation, or provider attempt.
    let replay = test::call_service(
        &first_app,
        owner_request(&invoke_uri, tenant_id, &owner_token)
            .set_json(invoke_body)
            .to_request(),
    )
    .await;
    assert_eq!(replay.status(), StatusCode::ACCEPTED);
    let replay_json: serde_json::Value =
        serde_json::from_slice(&test::read_body(replay).await).expect("replay JSON");
    assert_eq!(replay_json["invocation_id"], invocation_id.to_string());

    let durable_counts: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM object_invocations WHERE tenant_id=$1::uuid AND id=$2::uuid), (SELECT count(*) FROM invocation_idempotency WHERE tenant_id=$1::uuid AND invocation_id=$2::uuid), (SELECT count(*) FROM interaction_messages WHERE tenant_id=$1::uuid AND invocation_id=$2::uuid AND role='user'), (SELECT count(*) FROM outbox_messages WHERE tenant_id=$1::uuid AND payload->>'invocation_id'=$2), (SELECT count(*) FROM budget_reservations WHERE tenant_id=$1::uuid AND invocation_id=$2::uuid)",
    )
    .bind(tenant_id.to_string())
    .bind(invocation_id.to_string())
    .fetch_one(&first_pool)
    .await
    .expect("durable invocation side effects should be queryable");
    assert_eq!(durable_counts, (1, 1, 1, 1, 1));

    // Simulate process termination: discard the API router and close every
    // connection in the first pool. Only PostgreSQL retains the pending work.
    drop(first_app);
    first_pool.close().await;

    // Process 2: reconnect, re-authenticate, construct a fresh worker and provider.
    let second_pool = connect_pool(&database_url, PgPoolConfig::default())
        .await
        .expect("a restarted process should reconnect to PostgreSQL");
    run_migrations(&second_pool)
        .await
        .expect("existing migrations should remain valid after restart");

    let scope = authenticate_owner(
        &OwnerAuthenticator::new(second_pool.clone()),
        OwnerCredentials::new(tenant_id, owner_token.clone()),
    )
    .await
    .expect("persisted Owner credentials should reconstruct the trusted Scope");

    let provider_request = ChatRequest::new(
        MOCK_MODEL,
        vec![ChatMessage::new(ChatRole::User, USER_INPUT).expect("valid mock user message")],
        128,
    )
    .expect("valid mock provider request");
    let provider_response = ChatResponse::new(
        MOCK_OUTPUT,
        ChatFinishReason::Stop,
        Some(TokenUsage::new(12, 7)),
    )
    .expect("valid mock provider response");
    let provider = MockChatProvider::new()
        .with_response(provider_request, provider_response)
        .expect("the E2E request must have exactly one deterministic script");
    let worker_config = InvocationWorkerConfig::new(MOCK_MODEL, 128)
        .expect("valid worker config")
        .with_lease("walking-skeleton-after-restart", 60);
    let worker = InvocationWorker::new(
        PgExecutionStore::new(second_pool.clone()),
        provider,
        worker_config,
    );

    let outcome = worker
        .process(&scope, invocation_id)
        .await
        .expect("restarted worker should complete the persisted invocation");
    assert_eq!(
        outcome,
        InvocationWorkerOutcome::Completed {
            invocation_id,
            content: MOCK_OUTPUT.to_owned(),
        }
    );

    // Retrieve the durable result through the Owner HTTP API, not from worker memory.
    let second_app = test::init_service(configure_owner_routes(second_pool.clone())).await;
    let inspected = test::call_service(
        &second_app,
        test::TestRequest::get()
            .uri(&format!("/owner/invocations/{invocation_id}"))
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((
                axum::http::header::AUTHORIZATION,
                format!("Bearer {owner_token}"),
            ))
            .to_request(),
    )
    .await;
    assert_eq!(inspected.status(), StatusCode::OK);
    let inspected_body = test::read_body(inspected).await;
    assert!(
        !String::from_utf8_lossy(&inspected_body).contains(&owner_token),
        "Owner credential must never appear in invocation inspection"
    );
    let inspected_json: serde_json::Value =
        serde_json::from_slice(&inspected_body).expect("inspection JSON");
    assert_eq!(inspected_json["status"], "succeeded");
    assert_eq!(inspected_json["result"], MOCK_OUTPUT);
    assert_eq!(inspected_json["degraded"], false);

    // Another authenticated tenant cannot retrieve this Invocation.
    let cross_tenant = test::call_service(
        &second_app,
        test::TestRequest::get()
            .uri(&format!("/owner/invocations/{invocation_id}"))
            .insert_header(("X-Tenant-ID", other_tenant_id.to_string()))
            .insert_header((
                axum::http::header::AUTHORIZATION,
                format!("Bearer {other_owner_token}"),
            ))
            .to_request(),
    )
    .await;
    assert_eq!(cross_tenant.status(), StatusCode::NOT_FOUND);

    let persisted: (String, String, i64) = sqlx::query_as(
        "SELECT invocation.status, attempt.state, reservation.actual_tokens FROM object_invocations AS invocation JOIN provider_call_attempts AS attempt ON attempt.tenant_id=invocation.tenant_id AND attempt.object_id=invocation.object_id AND attempt.invocation_id=invocation.id JOIN budget_reservations AS reservation ON reservation.tenant_id=invocation.tenant_id AND reservation.object_id=invocation.object_id AND reservation.invocation_id=invocation.id WHERE invocation.tenant_id=$1::uuid AND invocation.id=$2::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(invocation_id.to_string())
    .fetch_one(&second_pool)
    .await
    .expect("terminal invocation, provider attempt and settlement should persist");
    assert_eq!(persisted.0, "Completed");
    assert_eq!(persisted.1, "ResponseReceived");
    assert_eq!(persisted.2, 19);

    let credential_hash: Vec<u8> = sqlx::query_scalar(
        "SELECT token_hash FROM owner_credentials WHERE tenant_id=$1::uuid AND caller_id=$2::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .fetch_one(&second_pool)
    .await
    .expect("only the credential digest should be persisted");
    assert_eq!(credential_hash, Sha256::digest(owner_token.as_bytes()).to_vec());

    let event_payloads: String = sqlx::query_scalar(
        "SELECT COALESCE(string_agg(payload::text, ''), '') FROM events WHERE tenant_id=$1::uuid",
    )
    .bind(tenant_id.to_string())
    .fetch_one(&second_pool)
    .await
    .expect("event payloads should be inspectable");
    let outbox_payloads: String = sqlx::query_scalar(
        "SELECT COALESCE(string_agg(payload::text, ''), '') FROM outbox_messages WHERE tenant_id=$1::uuid",
    )
    .bind(tenant_id.to_string())
    .fetch_one(&second_pool)
    .await
    .expect("outbox payloads should be inspectable");
    let persisted_messages: String = sqlx::query_scalar(
        "SELECT COALESCE(string_agg(content, ''), '') FROM interaction_messages WHERE tenant_id=$1::uuid",
    )
    .bind(tenant_id.to_string())
    .fetch_one(&second_pool)
    .await
    .expect("persisted conversation should be inspectable");
    for (surface, value) in [
        ("event", &event_payloads),
        ("outbox", &outbox_payloads),
        ("conversation/prompt source", &persisted_messages),
    ] {
        assert!(
            !value.contains(&owner_token),
            "Owner credential leaked into {surface}"
        );
    }

    // This API/runtime slice emits no tracing/log records and does not depend on
    // a logging facade; request credentials are never passed to a logger.

    drop(second_app);
    second_pool.close().await;
}
