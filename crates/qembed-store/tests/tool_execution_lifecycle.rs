mod common;

use qembed_domain::{
    ids::{
        CallerId, CommandId, InvocationId, ObjectId, ObjectVersionId, SnapshotId, TenantId,
        ToolExecutionId, TraceId,
    },
    invocation_lease::InvocationLeaseStore,
    test_support::scope_for_tenant,
    tool_execution::{
        ToolExecution, ToolExecutionCheckpoint, ToolExecutionFencingToken, ToolExecutionOutcome,
        ToolExecutionStatus,
    },
    tool_execution_lease::{
        ToolExecutionCompletion, ToolExecutionLeaseStore, ToolExecutionReconciliationStore,
    },
    tool_execution_reconciliation::{
        ToolExecutionReconciliationCompletion, ToolExecutionReconciliationOutcome,
        ToolExecutionReconciliationRequest,
    },
};
use qembed_store::{
    PgContext, invocation_worker::PgExecutionStore, run_migrations,
    tool_execution_store::PgToolExecutionStore,
};
use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    pool: PgPool,
    context: PgContext,
    scope: qembed_domain::scope::Scope,
    invocation_id: InvocationId,
    object_id: ObjectId,
    execution_id: ToolExecutionId,
}

async fn setup() -> Fixture {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("all store migrations should apply");

    let context = common::signed_context(&pool).await;
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
        "INSERT INTO object_tools (tenant_id, object_id, tool_id, tool_version_id, permissions)
         VALUES ($1, $2, $3, $4, '{}'::jsonb)",
    )
    .bind(tenant_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(tool_id)
    .bind(tool_version_id)
    .execute(&pool)
    .await
    .expect("current Object Tool binding exists");

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
        context,
        scope: scope_for_tenant(tenant_id),
        invocation_id,
        object_id,
        execution_id,
    }
}

#[tokio::test]
async fn changed_binding_version_rejects_claim_before_external_execution() {
    let fixture = setup().await;

    // Simulate an Owner changing the binding after the Allow decision and
    // ToolExecution were created. The trigger advances binding_version.
    sqlx::query(
        "UPDATE object_tools SET permissions=jsonb_build_object('updated', true)
          WHERE tenant_id=$1 AND object_id=$2",
    )
    .bind(fixture.scope.tenant_id().into_uuid())
    .bind(fixture.object_id.into_uuid())
    .execute(&fixture.pool)
    .await
    .expect("binding can be changed");

    let binding_state: (i64, bool) = sqlx::query_as(
        "SELECT binding_version, enabled FROM object_tools WHERE tenant_id=$1 AND object_id=$2",
    )
    .bind(fixture.scope.tenant_id().into_uuid())
    .bind(fixture.object_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("updated binding is readable");
    assert_eq!(
        binding_state,
        (2, true),
        "binding mutation advances its monotonic version"
    );

    let store = PgToolExecutionStore::new(fixture.context.clone());
    let claim = store
        .claim_tool_execution(&fixture.scope, fixture.execution_id, "tool-worker", 60)
        .await
        .expect("stale binding is a normal fail-closed claim miss");
    assert!(claim.is_none(), "stale capability must not acquire a lease");

    let persisted = store
        .load_tool_execution(&fixture.scope, fixture.execution_id)
        .await
        .expect("execution remains readable")
        .expect("execution exists");
    assert_eq!(persisted.status(), ToolExecutionStatus::Authorized);
    assert_eq!(persisted.version(), 0);
    assert_eq!(
        persisted.fencing_token(),
        ToolExecutionFencingToken::INITIAL
    );
    assert_eq!(persisted.lease_owner(), None);

    fixture.pool.close().await;
}

#[tokio::test]
async fn invocation_lease_does_not_authorize_tool_execution_result_write() {
    let fixture = setup().await;
    let invocation_store = PgExecutionStore::new(fixture.context.clone());
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

    let tool_store = PgToolExecutionStore::new(fixture.context.clone());
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
    let store = PgToolExecutionStore::new(fixture.context.clone());

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
    let heartbeat_state: (i64, i64, bool) = sqlx::query_as(
        "SELECT version, fencing_token, lease_until < clock_timestamp() + interval '90 seconds' FROM tool_executions WHERE id=$1",
    )
    .bind(fixture.execution_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("heartbeat state is readable");
    assert_eq!(heartbeat_state.0, 1);
    assert_eq!(heartbeat_state.1, 1);
    assert!(
        heartbeat_state.2,
        "repeated heartbeat must not add a full TTL to the previous expiry"
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

#[tokio::test]
async fn tool_execution_persistence_grants_match_the_runtime_capabilities() {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("all store migrations should apply");

    let privileges: (bool, bool, bool, bool, bool, bool, bool, bool) = sqlx::query_as(
        "SELECT
            has_table_privilege('qembed_runtime', 'tool_executions', 'SELECT'),
            has_table_privilege('qembed_runtime', 'tool_executions', 'INSERT'),
            has_table_privilege('qembed_runtime', 'tool_executions', 'UPDATE'),
            has_table_privilege('qembed_definer', 'tool_executions', 'SELECT'),
            has_table_privilege('qembed_definer', 'tool_executions', 'UPDATE'),
            has_table_privilege('qembed_dispatcher', 'tool_executions', 'SELECT'),
            has_table_privilege('qembed_admin', 'tool_executions', 'SELECT'),
            has_table_privilege('qembed_runtime', 'policy_decisions', 'UPDATE')",
    )
    .fetch_one(&pool)
    .await
    .expect("ToolExecution capability grants are queryable");

    assert_eq!(
        privileges,
        (true, true, true, true, true, false, false, false),
        "runtime owns execution writes; definer supports retention; unrelated roles cannot access executions"
    );

    pool.close().await;
}

#[tokio::test]
async fn expired_tool_execution_becomes_unknown_and_cannot_be_retried() {
    let fixture = setup().await;
    let store = PgToolExecutionStore::new(fixture.context.clone());
    let claimed = store
        .claim_tool_execution(&fixture.scope, fixture.execution_id, "tool-worker-a", 1)
        .await
        .expect("lease claim succeeds")
        .expect("Authorized execution is claimable");

    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;

    assert!(
        store
            .mark_tool_execution_unknown(
                &fixture.scope,
                fixture.execution_id,
                claimed.version(),
                claimed.fencing_token(),
            )
            .await
            .expect("expired lease transition query succeeds"),
        "the current expired lease checkpoint transitions exactly once"
    );

    let unknown = store
        .load_tool_execution(&fixture.scope, fixture.execution_id)
        .await
        .expect("Unknown execution can be loaded")
        .expect("execution exists");
    assert_eq!(unknown.status(), ToolExecutionStatus::Unknown);
    assert_eq!(unknown.version(), claimed.version() + 1);
    assert_eq!(unknown.fencing_token(), claimed.fencing_token());
    assert_eq!(unknown.lease_owner(), None);

    assert!(
        store
            .claim_tool_execution(&fixture.scope, fixture.execution_id, "tool-worker-b", 60)
            .await
            .expect("retry claim query succeeds")
            .is_none(),
        "Unknown must never return to Executing through the claim path"
    );

    let forbidden_retry = sqlx::query(
        "UPDATE tool_executions
            SET status='Executing', version=version+1, lease_owner='tool-worker-b',
                lease_until=clock_timestamp()+interval '60 seconds',
                heartbeat_at=clock_timestamp()
          WHERE id=$1",
    )
    .bind(fixture.execution_id.into_uuid())
    .execute(&fixture.pool)
    .await;
    assert!(
        forbidden_retry.is_err(),
        "the database state guard must reject Unknown -> Executing"
    );

    fixture.pool.close().await;
}

#[tokio::test]
async fn live_or_stale_tool_execution_checkpoint_cannot_be_marked_unknown() {
    let fixture = setup().await;
    let store = PgToolExecutionStore::new(fixture.context.clone());
    let claimed = store
        .claim_tool_execution(&fixture.scope, fixture.execution_id, "tool-worker-a", 60)
        .await
        .expect("lease claim succeeds")
        .expect("Authorized execution is claimable");

    assert!(
        !store
            .mark_tool_execution_unknown(
                &fixture.scope,
                fixture.execution_id,
                claimed.version(),
                claimed.fencing_token(),
            )
            .await
            .expect("live lease is not eligible for Unknown"),
        "a live lease must remain Executing"
    );

    assert!(
        !store
            .mark_tool_execution_unknown(
                &fixture.scope,
                fixture.execution_id,
                claimed.version().saturating_sub(1),
                claimed.fencing_token(),
            )
            .await
            .expect("stale version is a CAS miss"),
        "stale version cannot transition the row"
    );

    assert!(
        !store
            .mark_tool_execution_unknown(
                &fixture.scope,
                fixture.execution_id,
                claimed.version(),
                ToolExecutionFencingToken::from_value(
                    claimed.fencing_token().value().saturating_sub(1)
                ),
            )
            .await
            .expect("stale fencing token is a CAS miss"),
        "stale fencing token cannot transition the row"
    );

    let persisted = store
        .load_tool_execution(&fixture.scope, fixture.execution_id)
        .await
        .expect("execution remains readable")
        .expect("execution exists");
    assert_eq!(persisted.status(), ToolExecutionStatus::Executing);
    assert_eq!(persisted.version(), claimed.version());

    fixture.pool.close().await;
}

async fn transition_fixture_to_unknown(
    fixture: &Fixture,
    store: &PgToolExecutionStore,
) -> ToolExecution {
    let claimed = store
        .claim_tool_execution(&fixture.scope, fixture.execution_id, "reconcile-worker", 1)
        .await
        .expect("claim query succeeds")
        .expect("Authorized execution is claimable");
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    assert!(
        store
            .mark_tool_execution_unknown(
                &fixture.scope,
                fixture.execution_id,
                claimed.version(),
                claimed.fencing_token(),
            )
            .await
            .expect("expired lease transition succeeds")
    );
    store
        .load_tool_execution(&fixture.scope, fixture.execution_id)
        .await
        .expect("Unknown row can be loaded")
        .expect("execution exists")
}

fn reconciliation_request(execution: &ToolExecution) -> ToolExecutionReconciliationRequest {
    ToolExecutionReconciliationRequest::new(
        execution.id(),
        execution.invocation_id(),
        execution.command_id(),
        execution.version(),
        execution.fencing_token(),
    )
}

#[tokio::test]
async fn postgres_reconciliation_commits_confirmed_success_with_matching_state() {
    let fixture = setup().await;
    let store = PgToolExecutionStore::new(fixture.context.clone());
    let unknown = transition_fixture_to_unknown(&fixture, &store).await;
    let request = reconciliation_request(&unknown);
    let outcome = ToolExecutionReconciliationOutcome::ConfirmedSucceeded {
        output: Some(serde_json::json!({"provider":"confirmed"})),
        output_ttl_seconds: Some(3600),
    };
    let mut next = unknown.clone();
    next.reconcile(outcome.clone())
        .expect("confirmed success is valid");

    assert!(
        store
            .apply_tool_execution_reconciliation(
                &fixture.scope,
                ToolExecutionReconciliationCompletion {
                    request,
                    next,
                    outcome
                },
            )
            .await
            .expect("reconciliation CAS succeeds")
    );

    let persisted = store
        .load_tool_execution(&fixture.scope, fixture.execution_id)
        .await
        .expect("terminal row can be loaded")
        .expect("execution exists");
    assert_eq!(persisted.status(), ToolExecutionStatus::Succeeded);
    assert_eq!(persisted.version(), unknown.version() + 1);
    assert_eq!(persisted.fencing_token(), unknown.fencing_token());

    let state: (String, Option<serde_json::Value>) =
        sqlx::query_as("SELECT reconciliation_state, output FROM tool_executions WHERE id=$1")
            .bind(fixture.execution_id.into_uuid())
            .fetch_one(&fixture.pool)
            .await
            .expect("reconciliation evidence is persisted");
    assert_eq!(state.0, "confirmed_succeeded");
    assert_eq!(state.1, Some(serde_json::json!({"provider":"confirmed"})));
    fixture.pool.close().await;
}

#[tokio::test]
async fn postgres_reconciliation_commits_confirmed_failure() {
    let fixture = setup().await;
    let store = PgToolExecutionStore::new(fixture.context.clone());
    let unknown = transition_fixture_to_unknown(&fixture, &store).await;
    let request = reconciliation_request(&unknown);
    let outcome = ToolExecutionReconciliationOutcome::ConfirmedFailed {
        evidence: Some(serde_json::json!({"provider":"confirmed_absent"})),
    };
    let mut next = unknown.clone();
    next.reconcile(outcome.clone())
        .expect("confirmed failure is valid");

    assert!(
        store
            .apply_tool_execution_reconciliation(
                &fixture.scope,
                ToolExecutionReconciliationCompletion {
                    request,
                    next,
                    outcome
                },
            )
            .await
            .expect("reconciliation CAS succeeds")
    );

    let persisted = store
        .load_tool_execution(&fixture.scope, fixture.execution_id)
        .await
        .expect("terminal row can be loaded")
        .expect("execution exists");
    assert_eq!(persisted.status(), ToolExecutionStatus::Failed);
    assert_eq!(persisted.version(), unknown.version() + 1);
    let state: (String, Option<serde_json::Value>) =
        sqlx::query_as("SELECT reconciliation_state, output FROM tool_executions WHERE id=$1")
            .bind(fixture.execution_id.into_uuid())
            .fetch_one(&fixture.pool)
            .await
            .expect("failure evidence is persisted");
    assert_eq!(state.0, "confirmed_failed");
    assert_eq!(
        state.1,
        Some(serde_json::json!({"provider":"confirmed_absent"}))
    );
    fixture.pool.close().await;
}

#[tokio::test]
async fn postgres_reconciliation_rejects_stale_fencing_token() {
    let fixture = setup().await;
    let store = PgToolExecutionStore::new(fixture.context.clone());
    let unknown = transition_fixture_to_unknown(&fixture, &store).await;
    let stale_token =
        ToolExecutionFencingToken::from_value(unknown.fencing_token().value().saturating_sub(1));
    let outcome = ToolExecutionReconciliationOutcome::ConfirmedSucceeded {
        output: None,
        output_ttl_seconds: None,
    };
    let mut stale_next = ToolExecution::from_persisted(
        unknown.id(),
        unknown.tenant_id(),
        unknown.object_id(),
        unknown.invocation_id(),
        unknown.command_id(),
        *unknown.arguments_hash(),
        ToolExecutionCheckpoint::new(
            ToolExecutionStatus::Unknown,
            unknown.version(),
            stale_token,
            None,
        ),
    );
    stale_next
        .reconcile(outcome.clone())
        .expect("stale checkpoint can be represented");
    let request = ToolExecutionReconciliationRequest::new(
        unknown.id(),
        unknown.invocation_id(),
        unknown.command_id(),
        unknown.version(),
        stale_token,
    );

    assert!(
        !store
            .apply_tool_execution_reconciliation(
                &fixture.scope,
                ToolExecutionReconciliationCompletion {
                    request,
                    next: stale_next,
                    outcome
                },
            )
            .await
            .expect("stale fencing token is a CAS miss")
    );

    let persisted = store
        .load_tool_execution(&fixture.scope, fixture.execution_id)
        .await
        .expect("Unknown row remains readable")
        .expect("execution exists");
    assert_eq!(persisted.status(), ToolExecutionStatus::Unknown);
    assert_eq!(persisted.version(), unknown.version());
    assert_eq!(persisted.fencing_token(), unknown.fencing_token());
    fixture.pool.close().await;
}

#[tokio::test]
async fn postgres_reconciliation_rejects_mismatched_invocation_provenance() {
    let fixture = setup().await;
    let store = PgToolExecutionStore::new(fixture.context.clone());
    let unknown = transition_fixture_to_unknown(&fixture, &store).await;
    let forged_invocation_id = InvocationId::new();
    let outcome = ToolExecutionReconciliationOutcome::ConfirmedSucceeded {
        output: None,
        output_ttl_seconds: None,
    };
    let mut forged_next = ToolExecution::from_persisted(
        unknown.id(),
        unknown.tenant_id(),
        unknown.object_id(),
        forged_invocation_id,
        unknown.command_id(),
        ToolExecutionCheckpoint::new(
            ToolExecutionStatus::Unknown,
            unknown.version(),
            unknown.fencing_token(),
            None,
        ),
    );
    forged_next
        .reconcile(outcome.clone())
        .expect("forged provenance can be represented for the negative test");
    let request = ToolExecutionReconciliationRequest::new(
        unknown.id(),
        forged_invocation_id,
        unknown.command_id(),
        unknown.version(),
        unknown.fencing_token(),
    );

    assert!(
        !store
            .apply_tool_execution_reconciliation(
                &fixture.scope,
                ToolExecutionReconciliationCompletion {
                    request,
                    next: forged_next,
                    outcome,
                },
            )
            .await
            .expect("mismatched invocation provenance is a CAS miss")
    );

    let persisted = store
        .load_tool_execution(&fixture.scope, fixture.execution_id)
        .await
        .expect("Unknown row remains readable")
        .expect("execution exists");
    assert_eq!(persisted.status(), ToolExecutionStatus::Unknown);
    assert_eq!(persisted.version(), unknown.version());
    assert_eq!(persisted.invocation_id(), unknown.invocation_id());
    fixture.pool.close().await;
}

#[tokio::test]
async fn command_payload_store_persists_ciphertext_and_requires_matching_allow_hash() {
    use qembed_domain::{
        command_payload::{CommandPayloadStore, EncryptedCommandPayload},
        ids::CommandId,
    };
    use qembed_store::command_payload_store::PgCommandPayloadStore;

    let fixture = setup().await;
    let command_uuid: Uuid =
        sqlx::query_scalar("SELECT command_id FROM tool_executions WHERE id=$1")
            .bind(fixture.execution_id.into_uuid())
            .fetch_one(&fixture.pool)
            .await
            .expect("command ID exists");
    let command_id = CommandId::from_uuid(command_uuid);
    let store = PgCommandPayloadStore::new(fixture.context.clone());
    let encrypted = EncryptedCommandPayload::new(
        b"opaque-ciphertext-envelope".to_vec(),
        "test-kek-1".to_owned(),
        [2_u8; 32],
    );

    store
        .store_command_payload(&fixture.scope, command_id, encrypted.clone(), 300)
        .await
        .expect("matching Allow decision accepts encrypted payload");

    let persisted: (Vec<u8>, String, Vec<u8>) = sqlx::query_as(
        "SELECT payload_ciphertext, key_id, arguments_hash
           FROM tool_command_payloads WHERE tenant_id=$1 AND command_id=$2",
    )
    .bind(fixture.scope.tenant_id().into_uuid())
    .bind(command_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("ciphertext row exists");
    assert_eq!(persisted.0, b"opaque-ciphertext-envelope");
    assert_eq!(persisted.1, "test-kek-1");
    assert_eq!(persisted.2, vec![2_u8; 32]);

    let loaded = store
        .load_command_payload(&fixture.scope, command_id, [2_u8; 32])
        .await
        .expect("load succeeds")
        .expect("payload is visible for matching Allow hash");
    assert_eq!(loaded, encrypted);
    assert!(
        store
            .load_command_payload(&fixture.scope, command_id, [9_u8; 32])
            .await
            .expect("wrong digest is a fail-closed miss")
            .is_none()
    );

    fixture.pool.close().await;
}

async fn store_retention_test_payload(fixture: &Fixture, ttl_seconds: u32) -> CommandId {
    use qembed_domain::command_payload::{CommandPayloadStore, EncryptedCommandPayload};
    use qembed_store::command_payload_store::PgCommandPayloadStore;

    let command_uuid: Uuid =
        sqlx::query_scalar("SELECT command_id FROM tool_executions WHERE id=$1")
            .bind(fixture.execution_id.into_uuid())
            .fetch_one(&fixture.pool)
            .await
            .expect("command ID exists");
    let command_id = CommandId::from_uuid(command_uuid);
    PgCommandPayloadStore::new(fixture.context.clone())
        .store_command_payload(
            &fixture.scope,
            command_id,
            EncryptedCommandPayload::new(
                b"retention-test-ciphertext".to_vec(),
                "retention-test-key".to_owned(),
                [2_u8; 32],
            ),
            ttl_seconds,
        )
        .await
        .expect("encrypted command payload is stored");
    command_id
}

async fn purge_command_payloads_as_definer(pool: &PgPool, batch_size: i32) -> i64 {
    let mut transaction = pool.begin().await.expect("retention transaction begins");
    sqlx::query("SET LOCAL ROLE qembed_definer")
        .execute(&mut *transaction)
        .await
        .expect("retention worker assumes the dedicated definer role");
    let deleted =
        sqlx::query_scalar::<_, i64>("SELECT public.qembed_purge_tool_command_payloads($1)")
            .bind(batch_size)
            .fetch_one(&mut *transaction)
            .await
            .expect("bounded payload cleanup succeeds");
    transaction
        .commit()
        .await
        .expect("retention transaction commits");
    deleted
}

#[tokio::test]
async fn terminal_command_payload_cleanup_is_bounded_and_idempotent() {
    let fixture = setup().await;
    let command_id = store_retention_test_payload(&fixture, 3600).await;

    sqlx::query(
        "UPDATE tool_executions
            SET status='Failed', version=version+1, finished_at=clock_timestamp()
          WHERE tenant_id=$1 AND id=$2 AND status='Authorized'",
    )
    .bind(fixture.scope.tenant_id().into_uuid())
    .bind(fixture.execution_id.into_uuid())
    .execute(&fixture.pool)
    .await
    .expect("Authorized execution can be terminally failed");

    assert_eq!(
        purge_command_payloads_as_definer(&fixture.pool, 10).await,
        1,
        "terminal payload is removed even before its TTL"
    );
    assert_eq!(
        purge_command_payloads_as_definer(&fixture.pool, 10).await,
        0,
        "repeating cleanup is a no-op"
    );

    let payload_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tool_command_payloads WHERE tenant_id=$1 AND command_id=$2",
    )
    .bind(fixture.scope.tenant_id().into_uuid())
    .bind(command_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("payload absence is queryable");
    let execution_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tool_executions WHERE id=$1")
            .bind(fixture.execution_id.into_uuid())
            .fetch_one(&fixture.pool)
            .await
            .expect("execution ledger remains queryable");
    assert_eq!(payload_count, 0);
    assert_eq!(execution_count, 1, "cleanup never deletes audit history");

    fixture.pool.close().await;
}

#[tokio::test]
async fn command_payload_ttl_expires_even_without_terminal_execution() {
    let fixture = setup().await;
    let command_id = store_retention_test_payload(&fixture, 1).await;

    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;

    assert_eq!(
        purge_command_payloads_as_definer(&fixture.pool, 10).await,
        1
    );
    let status: String = sqlx::query_scalar("SELECT status FROM tool_executions WHERE id=$1")
        .bind(fixture.execution_id.into_uuid())
        .fetch_one(&fixture.pool)
        .await
        .expect("execution remains present");
    assert_eq!(status, "Authorized");
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tool_command_payloads WHERE tenant_id=$1 AND command_id=$2",
    )
    .bind(fixture.scope.tenant_id().into_uuid())
    .bind(command_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("expired payload count is queryable");
    assert_eq!(remaining, 0);

    fixture.pool.close().await;
}

#[tokio::test]
async fn unknown_command_payload_is_retained_until_reconciliation_or_ttl() {
    let fixture = setup().await;
    let command_id = store_retention_test_payload(&fixture, 3600).await;
    let store = PgToolExecutionStore::new(fixture.context.clone());
    let unknown = transition_fixture_to_unknown(&fixture, &store).await;

    assert_eq!(
        purge_command_payloads_as_definer(&fixture.pool, 10).await,
        0,
        "Unknown payload remains available while reconciliation is pending and TTL is live"
    );
    let retained: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tool_command_payloads WHERE tenant_id=$1 AND command_id=$2",
    )
    .bind(fixture.scope.tenant_id().into_uuid())
    .bind(command_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("Unknown payload is queryable");
    assert_eq!(retained, 1);

    let request = reconciliation_request(&unknown);
    let outcome = ToolExecutionReconciliationOutcome::ConfirmedFailed { evidence: None };
    let mut next = unknown;
    next.reconcile(outcome.clone())
        .expect("confirmed failure closes the Unknown execution");
    assert!(
        store
            .apply_tool_execution_reconciliation(
                &fixture.scope,
                ToolExecutionReconciliationCompletion {
                    request,
                    next,
                    outcome,
                },
            )
            .await
            .expect("confirmed reconciliation persists")
    );

    assert_eq!(
        purge_command_payloads_as_definer(&fixture.pool, 10).await,
        1,
        "payload becomes eligible immediately after confirmed terminal reconciliation"
    );
    assert_eq!(
        purge_command_payloads_as_definer(&fixture.pool, 10).await,
        0
    );

    fixture.pool.close().await;
}

#[tokio::test]
async fn unknown_command_payload_is_removed_after_ttl_when_reconciliation_is_pending() {
    let fixture = setup().await;
    let command_id = store_retention_test_payload(&fixture, 1).await;
    let store = PgToolExecutionStore::new(fixture.context.clone());
    let unknown = transition_fixture_to_unknown(&fixture, &store).await;
    assert_eq!(unknown.status(), ToolExecutionStatus::Unknown);

    assert_eq!(
        purge_command_payloads_as_definer(&fixture.pool, 10).await,
        1,
        "TTL is the upper bound even when reconciliation remains pending"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM tool_executions WHERE id=$1")
        .bind(fixture.execution_id.into_uuid())
        .fetch_one(&fixture.pool)
        .await
        .expect("Unknown execution remains in the audit ledger");
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tool_command_payloads WHERE tenant_id=$1 AND command_id=$2",
    )
    .bind(fixture.scope.tenant_id().into_uuid())
    .bind(command_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("expired Unknown payload count is queryable");
    assert_eq!(
        status, "Unknown",
        "cleanup does not rewrite reconciliation state"
    );
    assert_eq!(remaining, 0);

    fixture.pool.close().await;
}

#[tokio::test]
async fn command_payload_retention_privileges_are_narrow() {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("all store migrations should apply");

    // Runtime UPDATE is intentional: the tenant-scoped payload store needs it
    // for insert/replace and key rotation. RLS confines those writes to Scope.
    // Global DELETE and the privileged cleanup entry point remain definer-only.
    let privileges: (bool, bool, bool, bool, bool, bool, bool, bool, bool) = sqlx::query_as(
        "SELECT
            has_table_privilege('qembed_runtime', 'tool_command_payloads', 'DELETE'),
            has_table_privilege('qembed_definer', 'tool_command_payloads', 'DELETE'),
            has_column_privilege('qembed_runtime', 'tool_command_payloads', 'expires_at', 'UPDATE'),
            has_column_privilege('qembed_definer', 'tool_command_payloads', 'expires_at', 'UPDATE'),
            has_function_privilege('qembed_runtime',
                'public.qembed_purge_tool_command_payloads(integer)', 'EXECUTE'),
            has_function_privilege('qembed_definer',
                'public.qembed_purge_tool_command_payloads(integer)', 'EXECUTE'),
            (SELECT relrowsecurity FROM pg_class WHERE oid='public.tool_command_payloads'::regclass),
            (SELECT relforcerowsecurity FROM pg_class WHERE oid='public.tool_command_payloads'::regclass),
            EXISTS (
                SELECT 1 FROM pg_policies
                 WHERE schemaname='public'
                   AND tablename='tool_command_payloads'
                   AND policyname='tool_command_payloads_runtime_tenant'
                   AND qual LIKE '%app.tenant_id%'
            )",
    )
    .fetch_one(&pool)
    .await
    .expect("retention privileges and row-level security are queryable");
    assert_eq!(
        privileges,
        (false, true, true, true, false, true, true, true, true)
    );

    pool.close().await;
}

#[tokio::test]
async fn loaded_tool_execution_preserves_arguments_hash_separately_from_action_hash() {
    let fixture = setup().await;
    let store = PgToolExecutionStore::new(fixture.context.clone());

    let execution = store
        .load_tool_execution(&fixture.scope, fixture.execution_id)
        .await
        .expect("ToolExecution loads")
        .expect("fixture execution exists");

    // The fixture stores arguments_hash=[2; 32] and action_hash=[3; 32].
    // The executor must verify ciphertext against the former, not the latter.
    assert_eq!(execution.arguments_hash(), &[2_u8; 32]);

    fixture.pool.close().await;
}
