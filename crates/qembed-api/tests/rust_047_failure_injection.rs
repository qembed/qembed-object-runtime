//! RUST-047 failure-injection coverage for durable M1a invocation boundaries.
mod support;

use std::time::{Duration, SystemTime};

use axum::http::StatusCode;
use qembed_api::composition::configure_owner_routes;
use qembed_domain::{
    auth::authenticate_owner,
    caller::InvocationPool,
    chat::{
        ChatFinishReason, ChatMessage, ChatProviderError, ChatRequest, ChatResponse, ChatRole,
        TokenUsage,
    },
    ids::{CallerId, ObjectId, ObjectVersionId, PrincipalId, TenantId, TraceId},
    ports::{InvocationStartCommand, InvocationStartOutcome, InvocationStartStore},
};
use qembed_llm::mock::MockChatProvider;
use qembed_runtime::invocation_worker::{
    InvocationWorker, InvocationWorkerConfig, InvocationWorkerOutcome,
};
use qembed_store::{
    PgPoolConfig,
    auth::{OwnerAuthenticator, OwnerCredentials},
    connect_pool,
    invocation_start::PgInvocationStartStore,
    invocation_worker::PgExecutionStore,
    run_migrations,
};
use sha2::{Digest, Sha256};
use support::test;

const MODEL: &str = "test-chat";
const INPUT: &str = "RUST-047 durable failure injection input";
const OUTPUT: &str = "RUST-047 deterministic mock output";
const PAUSE_APPLICATION_NAME: &str = "qembed_rust047_outbox_pause";

// PostgreSQL triggers are database-wide, while Rust runs tests in this binary
// concurrently. Serialize the failpoint test with the restart test so the
// temporary outbox trigger cannot intercept another test's transaction.
static POSTGRES_FAILPOINT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fixture {
    pool: sqlx::PgPool,
    tenant_id: TenantId,
    owner_token: String,
    caller_id: CallerId,
    object_id: ObjectId,
}

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

async fn fixture() -> Fixture {
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
    let owner_token = format!("rust-047-owner-{}", PrincipalId::new());

    seed_owner(&pool, tenant_id, principal_id, &owner_token).await;

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
    let object_response = test::call_service(
        &app,
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
        &app,
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
        &app,
        owner_request(&publish_uri, tenant_id, &owner_token).to_request(),
    )
    .await;
    assert_eq!(publish_response.status(), StatusCode::OK);
    drop(app);

    sqlx::query(
        "INSERT INTO object_budgets (tenant_id, object_id, daily_token_limit, daily_cost_limit_micros, per_caller_daily_token_limit, max_concurrent_invocations, max_llm_calls, max_tool_calls, max_tokens_per_invocation, max_duration_ms) VALUES ($1::uuid, $2::uuid, 100000, 0, 100000, 10, 10, 0, 4096, 60000)",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(&pool)
    .await
    .expect("Object budget should be provisioned");

    Fixture {
        pool,
        tenant_id,
        owner_token,
        caller_id,
        object_id,
    }
}

async fn authenticated_scope(
    pool: &sqlx::PgPool,
    fixture: &Fixture,
) -> qembed_domain::scope::Scope {
    authenticate_owner(
        &OwnerAuthenticator::new(pool.clone()),
        OwnerCredentials::new(fixture.tenant_id, fixture.owner_token.clone()),
    )
    .await
    .expect("persisted Owner credentials should reconstruct trusted Scope")
}

fn start_command(object_id: ObjectId, caller_id: CallerId, key: &str) -> InvocationStartCommand {
    InvocationStartCommand {
        object_id,
        requested_caller_id: caller_id,
        invocation_pool: InvocationPool::Authenticated,
        idempotency_key: key.to_owned(),
        payload_hash: Sha256::digest(INPUT.as_bytes()).to_vec(),
        input_message: INPUT.to_owned(),
        message_expires_at: SystemTime::now() + Duration::from_secs(3600),
        idempotency_expires_at: SystemTime::now() + Duration::from_secs(86_400),
        trace_id: TraceId::new(),
    }
}

fn provider_request() -> ChatRequest {
    ChatRequest::new(
        MODEL,
        vec![ChatMessage::new(ChatRole::User, INPUT).expect("valid mock user message")],
        128,
    )
    .expect("valid mock provider request")
}

fn provider_response() -> ChatResponse {
    ChatResponse::new(OUTPUT, ChatFinishReason::Stop, Some(TokenUsage::new(12, 7)))
        .expect("valid mock provider response")
}

#[tokio::test]
async fn terminating_backend_before_commit_rolls_back_invocation_and_outbox_atomically() {
    let _serial = POSTGRES_FAILPOINT_TEST_LOCK.lock().await;
    let fixture = fixture().await;
    let scope = authenticated_scope(&fixture.pool, &fixture).await;

    sqlx::query("DROP TRIGGER IF EXISTS rust047_pause_outbox ON outbox_messages")
        .execute(&fixture.pool)
        .await
        .expect("stale outbox trigger should be removed");
    sqlx::query("DROP FUNCTION IF EXISTS rust047_pause_outbox_insert()")
        .execute(&fixture.pool)
        .await
        .expect("stale outbox trigger function should be removed");
    sqlx::query(
        &format!(
            "CREATE FUNCTION rust047_pause_outbox_insert() RETURNS trigger LANGUAGE plpgsql AS $rust047$ BEGIN PERFORM set_config('application_name', '{}', false); PERFORM pg_sleep(30); RETURN NEW; END; $rust047$",
            PAUSE_APPLICATION_NAME
        ),
    )
    .execute(&fixture.pool)
    .await
    .expect("pause trigger function should be created");
    sqlx::query(
        "CREATE TRIGGER rust047_pause_outbox BEFORE INSERT ON outbox_messages FOR EACH ROW EXECUTE FUNCTION rust047_pause_outbox_insert()",
    )
    .execute(&fixture.pool)
    .await
    .expect("outbox pause trigger should be created");

    let store = PgInvocationStartStore::new(fixture.pool.clone());
    let command = start_command(
        fixture.object_id,
        fixture.caller_id,
        "rust-047-precommit-crash",
    );
    let task = tokio::spawn(async move { store.begin_invocation(&scope, command).await });

    let mut backend_pid = None;
    for _ in 0..200 {
        backend_pid = sqlx::query_scalar::<_, i32>(
            "SELECT pid FROM pg_stat_activity WHERE application_name=$1 AND state='active' AND query LIKE '%INSERT INTO outbox_messages%' LIMIT 1",
        )
        .bind(PAUSE_APPLICATION_NAME)
        .fetch_optional(&fixture.pool)
        .await
        .expect("test should inspect PostgreSQL activity");
        if backend_pid.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let Some(backend_pid) = backend_pid else {
        task.abort();
        let _ = task.await;
        sqlx::query("DROP TRIGGER IF EXISTS rust047_pause_outbox ON outbox_messages")
            .execute(&fixture.pool)
            .await
            .expect("trigger cleanup");
        sqlx::query("DROP FUNCTION IF EXISTS rust047_pause_outbox_insert()")
            .execute(&fixture.pool)
            .await
            .expect("function cleanup");
        panic!("begin-invocation did not reach the outbox write failpoint");
    };

    let terminated = sqlx::query_scalar::<_, bool>("SELECT pg_terminate_backend($1)")
        .bind(backend_pid)
        .fetch_one(&fixture.pool)
        .await
        .expect("test should terminate the in-flight backend");
    assert!(terminated, "the pre-commit backend must be terminated");

    let result = task.await.expect("begin-invocation task should return");

    // Remove the database-wide failpoint before assertions so even a failed
    // assertion cannot leak it into later tests in this test process.
    sqlx::query("DROP TRIGGER IF EXISTS rust047_pause_outbox ON outbox_messages")
        .execute(&fixture.pool)
        .await
        .expect("outbox trigger should be removed");
    sqlx::query("DROP FUNCTION IF EXISTS rust047_pause_outbox_insert()")
        .execute(&fixture.pool)
        .await
        .expect("outbox trigger function should be removed");

    assert!(
        result.is_err(),
        "a killed transaction must not report success"
    );

    let counts: (i64, i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
          (SELECT count(*) FROM object_invocations WHERE tenant_id=$1::uuid AND object_id=$2::uuid),
          (SELECT count(*) FROM invocation_idempotency WHERE tenant_id=$1::uuid AND object_id=$2::uuid),
          (SELECT count(*) FROM interaction_messages WHERE tenant_id=$1::uuid AND object_id=$2::uuid),
          (SELECT count(*) FROM events WHERE tenant_id=$1::uuid AND event_type='invocation.received' AND payload->>'object_id'=$2),
          (SELECT count(*) FROM outbox_messages WHERE tenant_id=$1::uuid AND topic='invocation.received' AND payload->>'object_id'=$2),
          (SELECT count(*) FROM budget_reservations WHERE tenant_id=$1::uuid AND object_id=$2::uuid),
          (SELECT count(*) FROM provider_call_attempts WHERE tenant_id=$1::uuid AND object_id=$2::uuid),
          (SELECT count(*) FROM usage_counters WHERE tenant_id=$1::uuid AND object_id=$2::uuid),
          (SELECT count(*) FROM caller_usage_counters WHERE tenant_id=$1::uuid AND object_id=$2::uuid)",
    )
    .bind(fixture.tenant_id.to_string())
    .bind(fixture.object_id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .expect("all invocation side effects should be queryable");
    assert_eq!(counts, (0, 0, 0, 0, 0, 0, 0, 0, 0));

    fixture.pool.close().await;
}

#[tokio::test]
async fn committed_invocation_survives_restart_and_timeout_is_not_retried() {
    let _serial = POSTGRES_FAILPOINT_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = fixture().await;
    let scope = authenticated_scope(&fixture.pool, &fixture).await;
    let store = PgInvocationStartStore::new(fixture.pool.clone());
    let started = store
        .begin_invocation(
            &scope,
            start_command(
                fixture.object_id,
                fixture.caller_id,
                "rust-047-timeout-restart",
            ),
        )
        .await
        .expect("begin-invocation should commit");
    let invocation_id = match started {
        InvocationStartOutcome::Created { invocation_id } => invocation_id,
        InvocationStartOutcome::Replay { .. } => {
            panic!("fresh test key must create a new invocation")
        }
    };

    // Process teardown after commit: only PostgreSQL retains the pending work.
    fixture.pool.close().await;
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by the PostgreSQL integration-test job");
    let restarted_pool = connect_pool(&database_url, PgPoolConfig::default())
        .await
        .expect("restarted process should reconnect");
    run_migrations(&restarted_pool)
        .await
        .expect("migrations remain safe after restart");
    let restarted_scope = authenticated_scope(&restarted_pool, &fixture).await;

    let timeout_provider = MockChatProvider::new()
        .with_error(provider_request(), ChatProviderError::Timeout)
        .expect("timeout script should be unique");
    let timeout_worker = InvocationWorker::new(
        PgExecutionStore::new(restarted_pool.clone()),
        timeout_provider,
        InvocationWorkerConfig::new(MODEL, 128)
            .expect("valid worker config")
            .with_lease("rust-047-timeout-worker", 3),
    );
    let outcome = timeout_worker
        .process(&restarted_scope, invocation_id)
        .await
        .expect("ambiguous timeout should be persisted as unknown");
    assert_eq!(
        outcome,
        InvocationWorkerOutcome::OutcomeUnknown { invocation_id }
    );

    let state: (String, String, String, i64) = sqlx::query_as(
        "SELECT invocation.status, attempt.state, reservation.status, count(message.id)
         FROM object_invocations invocation
         JOIN provider_call_attempts attempt ON attempt.tenant_id=invocation.tenant_id AND attempt.object_id=invocation.object_id AND attempt.invocation_id=invocation.id
         JOIN budget_reservations reservation ON reservation.tenant_id=invocation.tenant_id AND reservation.object_id=invocation.object_id AND reservation.invocation_id=invocation.id
         LEFT JOIN interaction_messages message ON message.tenant_id=invocation.tenant_id AND message.object_id=invocation.object_id AND message.invocation_id=invocation.id AND message.role='assistant'
         WHERE invocation.tenant_id=$1::uuid AND invocation.id=$2::uuid
         GROUP BY invocation.status, attempt.state, reservation.status",
    )
    .bind(fixture.tenant_id.to_string())
    .bind(invocation_id.to_string())
    .fetch_one(&restarted_pool)
    .await
    .expect("timeout lifecycle state should be durable");
    assert_eq!(
        state,
        (
            "Processing".to_owned(),
            "OutcomeUnknown".to_owned(),
            "Reserved".to_owned(),
            0
        )
    );

    drop(timeout_worker);
    restarted_pool.close().await;

    // Wait for the first worker's lease to expire, then simulate a fresh worker process.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let recovered_pool = connect_pool(&database_url, PgPoolConfig::default())
        .await
        .expect("recovery process should reconnect");
    run_migrations(&recovered_pool)
        .await
        .expect("migrations remain safe on recovery");
    let recovered_scope = authenticated_scope(&recovered_pool, &fixture).await;
    let success_provider = MockChatProvider::new()
        .with_response(provider_request(), provider_response())
        .expect("success script should be unique");
    let recovery_worker = InvocationWorker::new(
        PgExecutionStore::new(recovered_pool.clone()),
        success_provider,
        InvocationWorkerConfig::new(MODEL, 128)
            .expect("valid recovery worker config")
            .with_lease("rust-047-recovery-worker", 3),
    );
    let recovered = recovery_worker
        .process(&recovered_scope, invocation_id)
        .await
        .expect("restart must preserve the ambiguous provider outcome");
    assert_eq!(
        recovered,
        InvocationWorkerOutcome::OutcomeUnknown { invocation_id }
    );

    let final_state: (String, String, i64) = sqlx::query_as(
        "SELECT invocation.status, attempt.state,
                (SELECT count(*) FROM interaction_messages message
                 WHERE message.tenant_id=invocation.tenant_id AND message.object_id=invocation.object_id
                   AND message.invocation_id=invocation.id AND message.role='assistant')
         FROM object_invocations invocation
         JOIN provider_call_attempts attempt ON attempt.tenant_id=invocation.tenant_id AND attempt.object_id=invocation.object_id AND attempt.invocation_id=invocation.id
         WHERE invocation.tenant_id=$1::uuid AND invocation.id=$2::uuid",
    )
    .bind(fixture.tenant_id.to_string())
    .bind(invocation_id.to_string())
    .fetch_one(&recovered_pool)
    .await
    .expect("recovered state should remain durable");
    assert_eq!(
        final_state,
        ("Processing".to_owned(), "OutcomeUnknown".to_owned(), 0)
    );

    recovered_pool.close().await;
}
