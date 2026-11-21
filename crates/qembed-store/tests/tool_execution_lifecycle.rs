mod common;

use qembed_domain::{
    ids::{
        CallerId, CommandId, InvocationId, ObjectId, ObjectVersionId, SnapshotId, TenantId,
        ToolExecutionId, TraceId,
    },
    invocation_lease::InvocationLeaseStore,
    test_support::scope_for_tenant,
    tool_execution::{ToolExecutionFencingToken, ToolExecutionOutcome, ToolExecutionStatus},
    tool_execution_lease::{ToolExecutionCompletion, ToolExecutionLeaseStore},
};
use qembed_store::{
    invocation_worker::PgExecutionStore, run_migrations, tool_execution_store::PgToolExecutionStore,
};
use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    pool: PgPool,
    scope: qembed_domain::scope::Scope,
    invocation_id: InvocationId,
    execution_id: ToolExecutionId,
}

async fn setup() -> Fixture {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("all store migrations should apply");

    let tenant_id = TenantId::new();
    let object_id = ObjectId::new();
    let caller_id = CallerId::new();
    let object_version_id = ObjectVersionId::new();
    let snapshot_id = SnapshotId::new();
    let invocation_id = InvocationId::new();
    let execution_id = ToolExecutionId::new();
    let command_id = CommandId::new();
    let trace_id = TraceId::new();
    let tool_id = Uuid::new_v4();
    let tool_version_id = Uuid::new_v4();
    let decision_id = Uuid::new_v4();

    sqlx::query("INSERT INTO tenants (id) VALUES ($1)")
        .bind(tenant_id.into_uuid())
        .execute(&pool)
        .await
        .expect("tenant exists");

    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind)
         VALUES ($1, $2, $3, 'Anonymous')",
    )
    .bind(caller_id.into_uuid())
    .bind(tenant_id.into_uuid())
    .bind(Uuid::new_v4().as_bytes().to_vec())
    .execute(&pool)
    .await
    .expect("caller exists");

    sqlx::query("INSERT INTO objects (tenant_id, object_id) VALUES ($1, $2)")
        .bind(tenant_id.into_uuid())
        .bind(object_id.into_uuid())
        .execute(&pool)
        .await
        .expect("object exists");

    sqlx::query(
        "INSERT INTO object_versions
            (tenant_id, object_id, object_version_id, version_number)
         VALUES ($1, $2, $3, 1)",
    )
    .bind(tenant_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(object_version_id.into_uuid())
    .execute(&pool)
    .await
    .expect("object version exists");

    sqlx::query(
        "INSERT INTO snapshots
            (tenant_id, snapshot_id, object_id, object_version_id, schema_version,
             canonical_json, content_hash, json_projection)
         VALUES ($1, $2, $3, $4, 1, $5, $6, $7)",
    )
    .bind(tenant_id.into_uuid())
    .bind(snapshot_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(object_version_id.into_uuid())
    .bind(br#"{"tool":"test"}"#.to_vec())
    .bind(vec![1_u8; 32])
    .bind(serde_json::json!({"tool":"test"}))
    .execute(&pool)
    .await
    .expect("snapshot exists");

    sqlx::query(
        "INSERT INTO object_invocations
            (id, tenant_id, object_id, caller_id, snapshot_id, knowledge_revision,
             pool, status, trace_id)
         VALUES ($1, $2, $3, $4, $5, 0, 'anonymous', 'Received', $6)",
    )
    .bind(invocation_id.into_uuid())
    .bind(tenant_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(caller_id.into_uuid())
    .bind(snapshot_id.into_uuid())
    .bind(trace_id.into_uuid())
    .execute(&pool)
    .await
    .expect("invocation exists");

    sqlx::query(
        "INSERT INTO tool_definitions (id, name, description)
         VALUES ($1, $2, 'RUST-053 integration fixture')",
    )
    .bind(tool_id)
    .bind(format!("tool-{}", Uuid::new_v4()))
    .execute(&pool)
    .await
    .expect("tool definition exists");

    sqlx::query(
        "INSERT INTO tool_versions
            (id, tool_id, version, risk_level, input_schema, output_schema, timeout_ms, idempotent)
         VALUES ($1, $2, '1', 'R1', '{}'::jsonb, '{}'::jsonb, 1000, true)",
    )
    .bind(tool_version_id)
    .bind(tool_id)
    .execute(&pool)
    .await
    .expect("tool version exists");

    sqlx::query(
        "INSERT INTO policy_decisions
            (id, tenant_id, object_id, invocation_id, caller_id, snapshot_id,
             tool_id, tool_version_id, binding_version, operation, arguments_hash,
             action_hash, risk_level, decision, policy_version, command_id, issued_at, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 1, 'test.operation',
                 $9, $10, 'R1', 'Allow', 'test-policy', $11, now(), now() + interval '5 minutes')",
    )
    .bind(decision_id)
    .bind(tenant_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(invocation_id.into_uuid())
    .bind(caller_id.into_uuid())
    .bind(snapshot_id.into_uuid())
    .bind(tool_id)
    .bind(tool_version_id)
    .bind(vec![2_u8; 32])
    .bind(vec![3_u8; 32])
    .bind(command_id.into_uuid())
    .execute(&pool)
    .await
    .expect("Allow decision exists");

    sqlx::query(
        "INSERT INTO tool_executions
            (id, tenant_id, object_id, invocation_id, caller_id, decision_id, command_id,
             tool_id, tool_version_id, snapshot_id, binding_version, operation,
             arguments_hash, status)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 1,
                 'test.operation', $11, 'Authorized')",
    )
    .bind(execution_id.into_uuid())
    .bind(tenant_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(invocation_id.into_uuid())
    .bind(caller_id.into_uuid())
    .bind(decision_id)
    .bind(command_id.into_uuid())
    .bind(tool_id)
    .bind(tool_version_id)
    .bind(snapshot_id.into_uuid())
    .bind(vec![2_u8; 32])
    .execute(&pool)
    .await
    .expect("Authorized ToolExecution exists");

    Fixture {
        pool,
        scope: scope_for_tenant(tenant_id),
        invocation_id,
        execution_id,
    }
}

#[tokio::test]
async fn invocation_lease_does_not_authorize_tool_execution_result_write() {
    let fixture = setup().await;
    let invocation_store = PgExecutionStore::new(fixture.pool.clone());
    let invocation = invocation_store
        .claim_invocation(
            &fixture.scope,
            fixture.invocation_id,
            "invocation-worker",
            60,
        )
        .await
        .expect("Invocation lease claim succeeds")
        .expect("Invocation is claimable");

    assert_eq!(invocation.version(), 0);
    assert_eq!(invocation.fencing_token().value(), 0);

    let tool_store = PgToolExecutionStore::new(fixture.pool.clone());
    let mut unpersisted_claim = tool_store
        .load_tool_execution(&fixture.scope, fixture.execution_id)
        .await
        .expect("ToolExecution can be loaded")
        .expect("ToolExecution exists");
    unpersisted_claim
        .claim("invocation-worker")
        .expect("the domain state machine can prepare a local claim");
    let expected_version = unpersisted_claim.version();
    let tool_fencing_token = unpersisted_claim.fencing_token();
    unpersisted_claim
        .complete(
            "invocation-worker",
            tool_fencing_token,
            ToolExecutionOutcome::Succeeded,
        )
        .expect("the local state machine can prepare a result");

    let wrote = tool_store
        .complete_tool_execution(
            &fixture.scope,
            ToolExecutionCompletion {
                execution_id: fixture.execution_id,
                owner: "invocation-worker".to_owned(),
                expected_version,
                expected_fencing_token: tool_fencing_token,
                next: unpersisted_claim,
                output: Some(serde_json::json!({"result":"forbidden"})),
                output_ttl_seconds: Some(60),
            },
        )
        .await
        .expect("a missing persisted ToolExecution lease is a CAS miss");

    assert!(
        !wrote,
        "Invocation ownership must not authorize ToolExecution writes"
    );

    let persisted = tool_store
        .load_tool_execution(&fixture.scope, fixture.execution_id)
        .await
        .expect("ToolExecution can be loaded")
        .expect("ToolExecution exists");
    assert_eq!(persisted.status(), ToolExecutionStatus::Authorized);
    assert_eq!(persisted.version(), 0);
    assert_eq!(
        persisted.fencing_token(),
        ToolExecutionFencingToken::INITIAL
    );

    fixture.pool.close().await;
}

#[tokio::test]
async fn concurrent_workers_get_one_tool_execution_lease_and_only_its_fence_can_commit() {
    let fixture = setup().await;
    let store = PgToolExecutionStore::new(fixture.pool.clone());

    let (first, second) = tokio::join!(
        store.claim_tool_execution(&fixture.scope, fixture.execution_id, "tool-worker-a", 60),
        store.claim_tool_execution(&fixture.scope, fixture.execution_id, "tool-worker-b", 60),
    );
    let first = first.expect("first claim query succeeds");
    let second = second.expect("second claim query succeeds");

    let (claimed, losing_owner) = match (first, second) {
        (Some(execution), None) => (execution, "tool-worker-b"),
        (None, Some(execution)) => (execution, "tool-worker-a"),
        (Some(_), Some(_)) => panic!("only one concurrent claim may win"),
        (None, None) => panic!("one concurrent claim must win"),
    };

    let owner = claimed
        .lease_owner()
        .expect("claimed ToolExecution has a lease owner");
    assert_eq!(claimed.status(), ToolExecutionStatus::Executing);
    assert_eq!(claimed.version(), 1);
    assert_eq!(claimed.fencing_token().value(), 1);

    assert!(
        store
            .heartbeat_tool_execution(
                &fixture.scope,
                fixture.execution_id,
                owner,
                claimed.version(),
                claimed.fencing_token(),
                60,
            )
            .await
            .expect("current owner heartbeat query succeeds")
    );
    assert!(
        !store
            .heartbeat_tool_execution(
                &fixture.scope,
                fixture.execution_id,
                losing_owner,
                claimed.version(),
                claimed.fencing_token(),
                60,
            )
            .await
            .expect("losing owner heartbeat query succeeds")
    );

    let output = serde_json::json!({"accepted":true});
    let mut next = claimed.clone();
    next.complete(
        owner,
        claimed.fencing_token(),
        ToolExecutionOutcome::Succeeded,
    )
    .expect("domain state machine accepts the current lease owner's result");

    assert!(
        store
            .complete_tool_execution(
                &fixture.scope,
                ToolExecutionCompletion {
                    execution_id: fixture.execution_id,
                    owner: owner.to_owned(),
                    expected_version: claimed.version(),
                    expected_fencing_token: claimed.fencing_token(),
                    next: next.clone(),
                    output: Some(output.clone()),
                    output_ttl_seconds: Some(3600),
                },
            )
            .await
            .expect("current ToolExecution lease can commit its result")
    );

    assert!(
        !store
            .complete_tool_execution(
                &fixture.scope,
                ToolExecutionCompletion {
                    execution_id: fixture.execution_id,
                    owner: owner.to_owned(),
                    expected_version: claimed.version(),
                    expected_fencing_token: claimed.fencing_token(),
                    next,
                    output: Some(output),
                    output_ttl_seconds: Some(3600),
                },
            )
            .await
            .expect("replayed result write is a CAS miss")
    );

    let persisted = store
        .load_tool_execution(&fixture.scope, fixture.execution_id)
        .await
        .expect("completed ToolExecution can be loaded")
        .expect("ToolExecution exists");
    assert_eq!(persisted.status(), ToolExecutionStatus::Succeeded);
    assert_eq!(persisted.version(), 2);
    assert_eq!(persisted.fencing_token(), claimed.fencing_token());
    assert_eq!(persisted.lease_owner(), None);

    let row: (serde_json::Value, Option<String>) =
        sqlx::query_as("SELECT output, output_expires_at::text FROM tool_executions WHERE id=$1")
            .bind(fixture.execution_id.into_uuid())
            .fetch_one(&fixture.pool)
            .await
            .expect("persisted output and expiry exist");
    assert_eq!(row.0, serde_json::json!({"accepted":true}));
    assert!(row.1.is_some());

    fixture.pool.close().await;
}
