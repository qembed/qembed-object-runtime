mod common;

use std::time::{Duration, SystemTime};

use qembed_domain::caller::InvocationPool;
use qembed_domain::ids::{
    CallerId, InvocationId, ObjectId, ObjectVersionId, SnapshotId, TenantId, TraceId,
};
use qembed_domain::invocation::{FencingToken, InvocationStatus};
use qembed_domain::invocation_lease::InvocationLeaseStore;
use qembed_domain::ports::{
    ExecutionStore, InvocationCancellationOutcome, InvocationCancellationStore,
    InvocationReadStore, InvocationWorkStore,
};
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::begin_invocation::{
    BeginInvocationCommand, BeginInvocationError, BeginInvocationOutcome, BudgetRejection,
    BudgetReservationError, begin_invocation,
};
use qembed_store::invocation_query::PgInvocationQueryStore;
use qembed_store::invocation_worker::PgExecutionStore;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

struct Fixture {
    pool: PgPool,
    admin_pool: PgPool,
    schema: String,
    object_id: ObjectId,
    caller_id: CallerId,
    snapshot_id: SnapshotId,
    scope: qembed_domain::scope::Scope,
}

async fn setup(fail_table: Option<&str>) -> Fixture {
    let admin_pool = common::connect().await;
    let schema = format!("begin_invocation_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .expect("isolated begin-invocation schema should be created");

    let ddl = [
        format!(
            "CREATE TABLE \"{schema}\".owner_principals (tenant_id UUID NOT NULL, caller_id UUID NOT NULL, principal_kind TEXT NOT NULL CHECK (principal_kind='tenant_owner'), disabled_at TIMESTAMPTZ, PRIMARY KEY (tenant_id, caller_id))"
        ),
        format!(
            "CREATE TABLE \"{schema}\".callers (id UUID PRIMARY KEY, tenant_id UUID NOT NULL, token_hash BYTEA NOT NULL, caller_kind TEXT NOT NULL CHECK (caller_kind IN ('Anonymous','Authenticated','Service')), revoked_at TIMESTAMPTZ, UNIQUE (tenant_id, id), UNIQUE (tenant_id, token_hash))"
        ),
        format!(
            "CREATE TABLE \"{schema}\".principal_caller_delegations (delegation_id UUID PRIMARY KEY, tenant_id UUID NOT NULL, principal_id UUID NOT NULL, caller_id UUID NOT NULL, revoked_at TIMESTAMPTZ, FOREIGN KEY (tenant_id, principal_id) REFERENCES \"{schema}\".owner_principals(tenant_id, caller_id), FOREIGN KEY (tenant_id, caller_id) REFERENCES \"{schema}\".callers(tenant_id, id))"
        ),
        format!(
            "CREATE TABLE \"{schema}\".objects (tenant_id UUID NOT NULL, object_id UUID NOT NULL, state TEXT NOT NULL, published_version_id UUID, PRIMARY KEY (tenant_id, object_id))"
        ),
        format!(
            "CREATE TABLE \"{schema}\".object_versions (tenant_id UUID NOT NULL, object_id UUID NOT NULL, object_version_id UUID NOT NULL, version_number BIGINT NOT NULL CHECK (version_number > 0), status TEXT NOT NULL, PRIMARY KEY (tenant_id, object_id, object_version_id), FOREIGN KEY (tenant_id, object_id) REFERENCES \"{schema}\".objects(tenant_id, object_id))"
        ),
        format!(
            "CREATE TABLE \"{schema}\".snapshots (tenant_id UUID NOT NULL, snapshot_id UUID NOT NULL, object_id UUID NOT NULL, object_version_id UUID NOT NULL, schema_version INTEGER NOT NULL CHECK (schema_version > 0), canonical_json BYTEA NOT NULL, content_hash BYTEA NOT NULL CHECK (octet_length(content_hash)=32), json_projection JSONB NOT NULL, PRIMARY KEY (tenant_id, snapshot_id), UNIQUE (tenant_id, object_id, snapshot_id), FOREIGN KEY (tenant_id, object_id, object_version_id) REFERENCES \"{schema}\".object_versions(tenant_id, object_id, object_version_id))"
        ),
        format!(
            "CREATE TABLE \"{schema}\".knowledge_revisions (tenant_id UUID NOT NULL, object_id UUID NOT NULL, current_revision BIGINT NOT NULL CHECK (current_revision >= 0), PRIMARY KEY (tenant_id, object_id), FOREIGN KEY (tenant_id, object_id) REFERENCES \"{schema}\".objects(tenant_id, object_id))"
        ),
        format!(
            "CREATE TABLE \"{schema}\".object_invocations (id UUID PRIMARY KEY, tenant_id UUID NOT NULL, object_id UUID NOT NULL, caller_id UUID NOT NULL, snapshot_id UUID NOT NULL, knowledge_revision BIGINT NOT NULL CHECK (knowledge_revision >= 0), pool TEXT NOT NULL CHECK (pool IN ('anonymous','authenticated')), status TEXT NOT NULL CHECK (status IN ('Received','ContextLoading','Processing','ExecutingTool','Verifying','WaitingForInput','WaitingForApproval','Completed','CompletedDegraded','Failed','Cancelled')), error_code TEXT, degraded_reason TEXT, trace_id UUID NOT NULL, cancel_requested_at TIMESTAMPTZ, lease_owner TEXT, lease_until TIMESTAMPTZ, heartbeat_at TIMESTAMPTZ, fencing_token BIGINT NOT NULL DEFAULT 0 CHECK (fencing_token >= 0), started_at TIMESTAMPTZ NOT NULL DEFAULT now(), finished_at TIMESTAMPTZ, version BIGINT NOT NULL DEFAULT 0 CHECK (version >= 0), CHECK ((status IN ('Completed','CompletedDegraded','Failed','Cancelled')) = (finished_at IS NOT NULL)), CHECK (status <> 'Failed' OR error_code IS NOT NULL), CHECK (status <> 'CompletedDegraded' OR degraded_reason IS NOT NULL), CHECK ((lease_owner IS NULL) = (lease_until IS NULL)), CHECK (finished_at IS NULL OR lease_owner IS NULL), UNIQUE (tenant_id, object_id, id), UNIQUE (tenant_id, object_id, caller_id, id), UNIQUE (tenant_id, object_id, id, pool), FOREIGN KEY (tenant_id, object_id) REFERENCES \"{schema}\".objects(tenant_id, object_id), FOREIGN KEY (tenant_id, caller_id) REFERENCES \"{schema}\".callers(tenant_id, id), FOREIGN KEY (tenant_id, object_id, snapshot_id) REFERENCES \"{schema}\".snapshots(tenant_id, object_id, snapshot_id))"
        ),
        format!(
            "CREATE TABLE \"{schema}\".object_budgets (tenant_id UUID NOT NULL, object_id UUID NOT NULL, daily_token_limit BIGINT NOT NULL CHECK (daily_token_limit >= 0), daily_cost_limit_micros BIGINT NOT NULL CHECK (daily_cost_limit_micros >= 0), anonymous_share_pct SMALLINT NOT NULL DEFAULT 50 CHECK (anonymous_share_pct BETWEEN 0 AND 100), per_caller_daily_token_limit BIGINT NOT NULL CHECK (per_caller_daily_token_limit >= 0), max_concurrent_invocations INTEGER NOT NULL CHECK (max_concurrent_invocations > 0), max_llm_calls INTEGER NOT NULL CHECK (max_llm_calls > 0), max_tool_calls INTEGER NOT NULL CHECK (max_tool_calls >= 0), max_tokens_per_invocation BIGINT NOT NULL CHECK (max_tokens_per_invocation > 0), max_duration_ms INTEGER NOT NULL CHECK (max_duration_ms > 0), updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), PRIMARY KEY (tenant_id, object_id), FOREIGN KEY (tenant_id, object_id) REFERENCES \"{schema}\".objects(tenant_id, object_id))"
        ),
        format!(
            "CREATE TABLE \"{schema}\".usage_counters (tenant_id UUID NOT NULL, object_id UUID NOT NULL, usage_date DATE NOT NULL, pool TEXT NOT NULL CHECK (pool IN ('anonymous','authenticated')), consumed_tokens BIGINT NOT NULL DEFAULT 0 CHECK (consumed_tokens >= 0), consumed_cost_micros BIGINT NOT NULL DEFAULT 0 CHECK (consumed_cost_micros >= 0), reserved_tokens BIGINT NOT NULL DEFAULT 0 CHECK (reserved_tokens >= 0), reserved_cost_micros BIGINT NOT NULL DEFAULT 0 CHECK (reserved_cost_micros >= 0), PRIMARY KEY (tenant_id, object_id, usage_date, pool), FOREIGN KEY (tenant_id, object_id) REFERENCES \"{schema}\".objects(tenant_id, object_id))"
        ),
        format!(
            "CREATE TABLE \"{schema}\".caller_usage_counters (tenant_id UUID NOT NULL, object_id UUID NOT NULL, caller_id UUID NOT NULL, usage_date DATE NOT NULL, consumed_tokens BIGINT NOT NULL DEFAULT 0 CHECK (consumed_tokens >= 0), reserved_tokens BIGINT NOT NULL DEFAULT 0 CHECK (reserved_tokens >= 0), PRIMARY KEY (tenant_id, object_id, caller_id, usage_date), FOREIGN KEY (tenant_id, object_id) REFERENCES \"{schema}\".objects(tenant_id, object_id), FOREIGN KEY (tenant_id, caller_id) REFERENCES \"{schema}\".callers(tenant_id, id) ON DELETE CASCADE)"
        ),
        format!(
            "CREATE TABLE \"{schema}\".budget_reservations (id UUID PRIMARY KEY, tenant_id UUID NOT NULL, object_id UUID NOT NULL, invocation_id UUID NOT NULL, pool TEXT NOT NULL CHECK (pool IN ('anonymous','authenticated')), usage_date DATE NOT NULL, reserved_tokens BIGINT NOT NULL CHECK (reserved_tokens >= 0), reserved_cost_micros BIGINT NOT NULL CHECK (reserved_cost_micros >= 0), actual_tokens BIGINT CHECK (actual_tokens >= 0), actual_cost_micros BIGINT CHECK (actual_cost_micros >= 0), status TEXT NOT NULL CHECK (status IN ('Reserved','Settled','Released','Expired')), expires_at TIMESTAMPTZ NOT NULL, UNIQUE (tenant_id, object_id, invocation_id), FOREIGN KEY (tenant_id, object_id, invocation_id, pool) REFERENCES \"{schema}\".object_invocations(tenant_id, object_id, id, pool) DEFERRABLE INITIALLY DEFERRED)"
        ),
        format!(
            "CREATE INDEX \"{schema}\".idx_resv_sweep ON \"{schema}\".budget_reservations (expires_at) WHERE status='Reserved'"
        ),
        format!(
            "CREATE TABLE \"{schema}\".interaction_messages (id UUID PRIMARY KEY, tenant_id UUID NOT NULL, object_id UUID NOT NULL, caller_id UUID NOT NULL, invocation_id UUID NOT NULL, role TEXT NOT NULL CHECK (role IN ('user','assistant','tool')), content TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now(), expires_at TIMESTAMPTZ NOT NULL, FOREIGN KEY (tenant_id, object_id, caller_id, invocation_id) REFERENCES \"{schema}\".object_invocations(tenant_id, object_id, caller_id, id))"
        ),
        format!(
            "CREATE TABLE \"{schema}\".invocation_idempotency (tenant_id UUID NOT NULL, object_id UUID NOT NULL, caller_id UUID NOT NULL, idempotency_key TEXT NOT NULL, payload_hash BYTEA NOT NULL, invocation_id UUID NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now(), expires_at TIMESTAMPTZ NOT NULL, PRIMARY KEY (tenant_id, object_id, caller_id, idempotency_key), UNIQUE (tenant_id, object_id, invocation_id), FOREIGN KEY (tenant_id, object_id, caller_id, invocation_id) REFERENCES \"{schema}\".object_invocations(tenant_id, object_id, caller_id, id) DEFERRABLE INITIALLY DEFERRED)"
        ),
        format!(
            "CREATE TABLE \"{schema}\".events (tenant_id UUID NOT NULL, event_id UUID NOT NULL, aggregate_type TEXT NOT NULL, aggregate_id UUID NOT NULL, event_type TEXT NOT NULL, payload JSONB NOT NULL, occurred_at TIMESTAMPTZ NOT NULL DEFAULT now(), PRIMARY KEY (tenant_id, event_id), UNIQUE (event_id))"
        ),
        format!(
            "CREATE TABLE \"{schema}\".outbox_messages (tenant_id UUID NOT NULL, delivery_id UUID NOT NULL, event_id UUID NOT NULL, topic TEXT NOT NULL, payload JSONB NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now(), delivered_at TIMESTAMPTZ, PRIMARY KEY (tenant_id, delivery_id), UNIQUE (tenant_id, event_id), FOREIGN KEY (tenant_id, event_id) REFERENCES \"{schema}\".events(tenant_id, event_id))"
        ),
    ];
    for statement in ddl {
        sqlx::query(&statement)
            .execute(&admin_pool)
            .await
            .expect("canonical table fixture should be created");
    }

    let resolver = format!(
        r#"CREATE FUNCTION "{schema}".resolve_delegated_caller(p_caller_id UUID)
        RETURNS TABLE (delegation_id UUID, caller_id UUID, caller_kind TEXT)
        LANGUAGE plpgsql AS $$
        DECLARE v_tenant UUID := NULLIF(current_setting('app.tenant_id', true), '')::uuid;
                v_principal UUID := NULLIF(current_setting('app.principal_id', true), '')::uuid;
        BEGIN
          RETURN QUERY
          SELECT d.delegation_id, c.id, c.caller_kind
            FROM "{schema}".principal_caller_delegations d
            JOIN "{schema}".owner_principals p
              ON p.tenant_id=d.tenant_id AND p.caller_id=d.principal_id
            JOIN "{schema}".callers c
              ON c.tenant_id=d.tenant_id AND c.id=d.caller_id
           WHERE d.tenant_id=v_tenant AND d.principal_id=v_principal
             AND d.caller_id=p_caller_id AND d.revoked_at IS NULL
             AND p.disabled_at IS NULL AND c.revoked_at IS NULL
           FOR SHARE OF d,p,c;
          IF NOT FOUND THEN
            RAISE EXCEPTION 'no active delegation' USING ERRCODE='42501';
          END IF;
        END $$"#
    );
    sqlx::query(&resolver)
        .execute(&admin_pool)
        .await
        .expect("locked delegation resolver should be created");

    if let Some(table) = fail_table {
        let function = format!(
            r#"CREATE FUNCTION "{schema}".fail_insert() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN RAISE EXCEPTION 'injected {table} failure'; END $$"#
        );
        sqlx::query(&function)
            .execute(&admin_pool)
            .await
            .expect("failure trigger function should be created");
        let trigger = format!(
            "CREATE TRIGGER injected_failure BEFORE INSERT ON \"{schema}\".{table} FOR EACH ROW EXECUTE FUNCTION \"{schema}\".fail_insert()"
        );
        sqlx::query(&trigger)
            .execute(&admin_pool)
            .await
            .expect("failure trigger should be created");
    }

    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by scripts/test-postgres.sh");
    let pool_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |connection, _| {
            let schema = pool_schema.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO \"{schema}\""))
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .expect("schema-bound test pool should connect");

    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let object_id = ObjectId::new();
    let caller_id = CallerId::new();
    let version_id = ObjectVersionId::new();
    let snapshot_id = SnapshotId::new();

    sqlx::query("INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) VALUES ($1::uuid, $2::uuid, 'tenant_owner')")
        .bind(tenant_id.to_string())
        .bind(scope.principal().principal_id().to_string())
        .execute(&pool).await.expect("Principal fixture should be inserted");
    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind) VALUES ($1::uuid, $2::uuid, $3, 'Service')",
    )
    .bind(caller_id.to_string())
    .bind(tenant_id.to_string())
    .bind(caller_id.into_uuid().as_bytes().to_vec())
    .execute(&pool)
    .await
    .expect("Service Caller fixture should be inserted");
    sqlx::query("INSERT INTO principal_caller_delegations (delegation_id, tenant_id, principal_id, caller_id) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid)")
        .bind(Uuid::new_v4().to_string()).bind(tenant_id.to_string())
        .bind(scope.principal().principal_id().to_string()).bind(caller_id.to_string())
        .execute(&pool).await.expect("explicit delegation fixture should be inserted");
    sqlx::query("INSERT INTO objects (tenant_id, object_id, state, published_version_id) VALUES ($1::uuid, $2::uuid, 'active', $3::uuid)")
        .bind(tenant_id.to_string()).bind(object_id.to_string()).bind(version_id.to_string())
        .execute(&pool).await.expect("active object fixture should be inserted");
    sqlx::query("INSERT INTO object_budgets (tenant_id, object_id, daily_token_limit, daily_cost_limit_micros, per_caller_daily_token_limit, max_concurrent_invocations, max_llm_calls, max_tool_calls, max_tokens_per_invocation, max_duration_ms) VALUES ($1::uuid, $2::uuid, 100000, 0, 100000, 10, 10, 0, 10000, 60000)")
        .bind(tenant_id.to_string()).bind(object_id.to_string())
        .execute(&pool).await.expect("object budget fixture should be inserted");
    sqlx::query("INSERT INTO object_versions (tenant_id, object_id, object_version_id, version_number, status) VALUES ($1::uuid, $2::uuid, $3::uuid, 1, 'published')")
        .bind(tenant_id.to_string()).bind(object_id.to_string()).bind(version_id.to_string())
        .execute(&pool).await.expect("published version fixture should be inserted");
    sqlx::query("INSERT INTO snapshots (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, convert_to('{\"limits\":{\"max_tokens_per_invocation\":10000}}','UTF8'), decode(repeat('00',32),'hex'), '{\"limits\":{\"max_tokens_per_invocation\":10000}}'::jsonb)")
        .bind(tenant_id.to_string()).bind(snapshot_id.to_string()).bind(object_id.to_string()).bind(version_id.to_string())
        .execute(&pool).await.expect("snapshot fixture should be inserted");
    sqlx::query("INSERT INTO knowledge_revisions (tenant_id, object_id, current_revision) VALUES ($1::uuid, $2::uuid, 7)")
        .bind(tenant_id.to_string()).bind(object_id.to_string())
        .execute(&pool).await.expect("knowledge revision fixture should be inserted");

    Fixture {
        pool,
        admin_pool,
        schema,
        object_id,
        caller_id,
        snapshot_id,
        scope,
    }
}

fn command(fixture: &Fixture, key: &str) -> BeginInvocationCommand {
    BeginInvocationCommand {
        object_id: fixture.object_id,
        requested_caller_id: fixture.caller_id,
        invocation_pool: InvocationPool::Authenticated,
        snapshot_id: fixture.snapshot_id,
        idempotency_key: key.to_owned(),
        payload_hash: b"request-digest".to_vec(),
        input_message: "hello from durable invocation input".to_owned(),
        message_expires_at: SystemTime::now() + Duration::from_secs(86_400),
        idempotency_expires_at: SystemTime::now() + Duration::from_secs(3_600),
        trace_id: TraceId::new(),
    }
}

async fn counts(fixture: &Fixture) -> (i64, i64, i64, i64, i64) {
    let invocations = sqlx::query_scalar("SELECT count(*) FROM object_invocations")
        .fetch_one(&fixture.pool)
        .await
        .expect("invocation count");
    let idempotency = sqlx::query_scalar("SELECT count(*) FROM invocation_idempotency")
        .fetch_one(&fixture.pool)
        .await
        .expect("idempotency count");
    let events = sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(&fixture.pool)
        .await
        .expect("event count");
    let outbox = sqlx::query_scalar("SELECT count(*) FROM outbox_messages")
        .fetch_one(&fixture.pool)
        .await
        .expect("outbox count");
    let messages = sqlx::query_scalar("SELECT count(*) FROM interaction_messages")
        .fetch_one(&fixture.pool)
        .await
        .expect("interaction message count");
    (invocations, idempotency, events, outbox, messages)
}

async fn teardown(fixture: Fixture) {
    fixture.pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{}\" CASCADE", fixture.schema))
        .execute(&fixture.admin_pool)
        .await
        .expect("fixture schema should be dropped");
    fixture.admin_pool.close().await;
}

#[tokio::test]
async fn begin_invocation_commits_all_rows_and_replays_by_effective_caller() {
    let fixture = setup(None).await;
    let outcome = begin_invocation(&fixture.pool, &fixture.scope, command(&fixture, "key-1"))
        .await
        .expect("begin invocation should commit");
    let invocation_id = match outcome {
        BeginInvocationOutcome::Created {
            invocation_id,
            caller_id,
            knowledge_revision,
            caller_kind,
            invocation_pool,
            ..
        } => {
            assert_eq!(caller_id, fixture.caller_id);
            assert_eq!(caller_kind, qembed_domain::caller::CallerKind::Service);
            assert_eq!(knowledge_revision, 7);
            assert_eq!(invocation_pool, InvocationPool::Authenticated);
            invocation_id
        }
        other => panic!("expected Created, got {other:?}"),
    };
    assert_eq!(counts(&fixture).await, (1, 1, 1, 1, 1));
    let reservation_count: i64 = sqlx::query_scalar("SELECT count(*) FROM budget_reservations")
        .fetch_one(&fixture.pool).await.expect("reservation count");
    assert_eq!(reservation_count, 1, "new invocation has exactly one reservation");
    let object_reserved: i64 = sqlx::query_scalar("SELECT sum(reserved_tokens) FROM usage_counters")
        .fetch_one(&fixture.pool).await.expect("object reserved tokens");
    let caller_reserved: i64 = sqlx::query_scalar("SELECT sum(reserved_tokens) FROM caller_usage_counters")
        .fetch_one(&fixture.pool).await.expect("caller reserved tokens");
    assert!(object_reserved > 0);
    assert_eq!(caller_reserved, object_reserved);


    let persisted = sqlx::query_as::<_, (String, String, i64, String)>(
        "SELECT caller_id::text, snapshot_id::text, knowledge_revision, pool FROM object_invocations WHERE id=$1::uuid"
    ).bind(invocation_id.to_string()).fetch_one(&fixture.pool).await.expect("invocation should be persisted");
    assert_eq!(persisted.0, fixture.caller_id.to_string());
    assert_eq!(persisted.1, fixture.snapshot_id.to_string());
    assert_eq!(persisted.2, 7);
    assert_eq!(persisted.3, "authenticated");
    assert_ne!(
        fixture.scope.principal().principal_id().into_uuid(),
        fixture.caller_id.into_uuid(),
        "Principal and Caller identities must not rely on UUID equality"
    );
    let input_message: String = sqlx::query_scalar(
        "SELECT content FROM interaction_messages WHERE invocation_id=$1::uuid AND role='user'",
    )
    .bind(invocation_id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .expect("original user input should be durable");
    assert_eq!(input_message, "hello from durable invocation input");

    let event_type: String = sqlx::query_scalar("SELECT payload->>'type' FROM events")
        .fetch_one(&fixture.pool)
        .await
        .expect("event payload should include its allowlisted type");
    assert_eq!(event_type, "invocation_received");

    let mut replay_command = command(&fixture, "key-1");
    replay_command.message_expires_at = std::time::UNIX_EPOCH;
    let replay = begin_invocation(&fixture.pool, &fixture.scope, replay_command)
        .await
        .expect("same key and payload should replay without validating a new message expiry");
    assert_eq!(replay, BeginInvocationOutcome::Replay { invocation_id });
    assert_eq!(counts(&fixture).await, (1, 1, 1, 1, 1));
    teardown(fixture).await;
}

#[tokio::test]
async fn any_failure_after_idempotency_claim_rolls_back_the_entire_begin() {
    // Snapshot validation fails after the key has been reserved.
    let fixture = setup(None).await;
    let mut invalid = command(&fixture, "missing-snapshot");
    invalid.snapshot_id = SnapshotId::new();
    let error = begin_invocation(&fixture.pool, &fixture.scope, invalid)
        .await
        .expect_err("missing snapshot must fail");
    assert!(matches!(error, BeginInvocationError::SnapshotNotFound));
    assert_eq!(counts(&fixture).await, (0, 0, 0, 0, 0));
    teardown(fixture).await;
}

#[tokio::test]
async fn invocation_insert_failure_rolls_back_idempotency_claim() {
    let fixture = setup(Some("object_invocations")).await;
    let result = begin_invocation(
        &fixture.pool,
        &fixture.scope,
        command(&fixture, "fail-invocation"),
    )
    .await;
    assert!(matches!(result, Err(BeginInvocationError::Database(_))));
    assert_eq!(counts(&fixture).await, (0, 0, 0, 0, 0));
    teardown(fixture).await;
}

#[tokio::test]
async fn event_insert_failure_rolls_back_invocation_and_idempotency() {
    let fixture = setup(Some("events")).await;
    let result = begin_invocation(
        &fixture.pool,
        &fixture.scope,
        command(&fixture, "fail-event"),
    )
    .await;
    assert!(matches!(result, Err(BeginInvocationError::Database(_))));
    assert_eq!(counts(&fixture).await, (0, 0, 0, 0, 0));
    teardown(fixture).await;
}

#[tokio::test]
async fn outbox_insert_failure_rolls_back_invocation_idempotency_and_event() {
    let fixture = setup(Some("outbox_messages")).await;
    let result = begin_invocation(
        &fixture.pool,
        &fixture.scope,
        command(&fixture, "fail-outbox"),
    )
    .await;
    assert!(matches!(result, Err(BeginInvocationError::Database(_))));
    assert_eq!(counts(&fixture).await, (0, 0, 0, 0, 0));
    teardown(fixture).await;
}

#[tokio::test]
async fn principal_uuid_is_not_an_implicit_caller_mapping() {
    let fixture = setup(None).await;
    let mut invalid = command(&fixture, "principal-is-not-caller");
    invalid.requested_caller_id =
        CallerId::from_uuid(fixture.scope.principal().principal_id().into_uuid());
    let error = begin_invocation(&fixture.pool, &fixture.scope, invalid)
        .await
        .expect_err("Principal UUID equality must not authorize a Caller");
    assert!(matches!(
        error,
        BeginInvocationError::CallerResolution(
            qembed_store::caller::CallerResolutionError::NotDelegated
        )
    ));
    assert_eq!(counts(&fixture).await, (0, 0, 0, 0, 0));
    teardown(fixture).await;
}

#[tokio::test]
async fn missing_knowledge_revision_rolls_back_the_idempotency_claim() {
    let fixture = setup(None).await;
    sqlx::query("DELETE FROM knowledge_revisions WHERE object_id=$1::uuid")
        .bind(fixture.object_id.to_string())
        .execute(&fixture.pool)
        .await
        .expect("knowledge revision fixture should be removed");
    let error = begin_invocation(
        &fixture.pool,
        &fixture.scope,
        command(&fixture, "missing-revision"),
    )
    .await
    .expect_err("missing knowledge revision must fail");
    assert!(matches!(
        error,
        BeginInvocationError::KnowledgeRevisionNotFound
    ));
    assert_eq!(counts(&fixture).await, (0, 0, 0, 0, 0));
    teardown(fixture).await;
}

#[tokio::test]
async fn inactive_object_rolls_back_the_idempotency_claim() {
    let fixture = setup(None).await;
    sqlx::query("UPDATE objects SET state='suspended' WHERE object_id=$1::uuid")
        .bind(fixture.object_id.to_string())
        .execute(&fixture.pool)
        .await
        .expect("object should be suspended");
    let error = begin_invocation(
        &fixture.pool,
        &fixture.scope,
        command(&fixture, "inactive-object"),
    )
    .await
    .expect_err("suspended object must not begin an invocation");
    assert!(matches!(error, BeginInvocationError::ObjectNotActive));
    assert_eq!(counts(&fixture).await, (0, 0, 0, 0, 0));
    teardown(fixture).await;
}

#[tokio::test]
async fn worker_store_persists_response_and_resumes_after_process_restart() {
    let fixture = setup(None).await;
    let invocation_id = match begin_invocation(
        &fixture.pool,
        &fixture.scope,
        command(&fixture, "worker-restart"),
    )
    .await
    .expect("begin invocation")
    {
        BeginInvocationOutcome::Created { invocation_id, .. } => invocation_id,
        other => panic!("expected new invocation, got {other:?}"),
    };

    let store = PgExecutionStore::new(fixture.pool.clone());
    let input = store
        .load_user_input(&fixture.scope, invocation_id)
        .await
        .expect("input query")
        .expect("durable user input");
    assert_eq!(input, "hello from durable invocation input");

    let mut current = store
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("load invocation")
        .expect("invocation exists");
    assert_eq!(current.status(), InvocationStatus::Received);

    let mut next = current.clone();
    next.transition_to(InvocationStatus::ContextLoading, FencingToken::INITIAL)
        .expect("Received -> ContextLoading");
    assert!(
        store
            .compare_and_set_invocation(
                &fixture.scope,
                current.version(),
                FencingToken::INITIAL,
                next,
            )
            .await
            .expect("persist context-loading state")
    );

    current = store
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("reload invocation")
        .expect("invocation exists");
    let mut next = current.clone();
    next.transition_to(InvocationStatus::Processing, FencingToken::INITIAL)
        .expect("ContextLoading -> Processing");
    assert!(
        store
            .compare_and_set_invocation(
                &fixture.scope,
                current.version(),
                FencingToken::INITIAL,
                next,
            )
            .await
            .expect("persist processing state")
    );

    current = store
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("reload processing invocation")
        .expect("invocation exists");
    let mut next = current.clone();
    next.transition_to(InvocationStatus::Verifying, FencingToken::INITIAL)
        .expect("Processing -> Verifying");
    assert!(
        store
            .persist_assistant_output(
                &fixture.scope,
                current.version(),
                FencingToken::INITIAL,
                next,
                "durable answer".to_owned(),
            )
            .await
            .expect("persist response and verifying state")
    );

    // A new adapter instance represents a restarted worker process.
    let restarted = PgExecutionStore::new(fixture.pool.clone());
    let recovered = restarted
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("reload after restart")
        .expect("invocation exists");
    assert_eq!(recovered.status(), InvocationStatus::Verifying);
    let output = restarted
        .load_assistant_output(&fixture.scope, invocation_id)
        .await
        .expect("load persisted response")
        .expect("assistant output survived restart");
    assert_eq!(output, "durable answer");

    let mut completed = recovered.clone();
    completed
        .transition_to(InvocationStatus::Completed, FencingToken::INITIAL)
        .expect("Verifying -> Completed");
    assert!(
        restarted
            .complete_invocation(
                &fixture.scope,
                recovered.version(),
                FencingToken::INITIAL,
                completed,
            )
            .await
            .expect("persist terminal state")
    );

    let terminal = restarted
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("load terminal invocation")
        .expect("terminal invocation exists");
    assert_eq!(terminal.status(), InvocationStatus::Completed);
    assert_eq!(
        restarted
            .load_assistant_output(&fixture.scope, invocation_id)
            .await
            .expect("load terminal output")
            .as_deref(),
        Some("durable answer")
    );

    teardown(fixture).await;
}

#[tokio::test]
async fn invalid_new_input_rolls_back_idempotency_and_all_begin_rows() {
    let fixture = setup(None).await;
    let mut invalid = command(&fixture, "invalid-input");
    invalid.input_message = "   ".to_owned();

    let error = begin_invocation(&fixture.pool, &fixture.scope, invalid)
        .await
        .expect_err("blank user input must be rejected");
    assert!(matches!(error, BeginInvocationError::InvalidInputMessage));
    assert_eq!(counts(&fixture).await, (0, 0, 0, 0, 0));
    teardown(fixture).await;
}

#[tokio::test]
async fn invocation_lease_reclaim_fences_the_previous_worker_across_sessions() {
    let fixture = setup(None).await;
    let invocation_id = match begin_invocation(
        &fixture.pool,
        &fixture.scope,
        command(&fixture, "lease-fencing"),
    )
    .await
    .expect("begin invocation")
    {
        BeginInvocationOutcome::Created { invocation_id, .. } => invocation_id,
        other => panic!("expected new invocation, got {other:?}"),
    };

    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by scripts/test-postgres.sh");
    let schema = fixture.schema.clone();
    let second_pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |connection, _| {
            let schema = schema.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO \"{schema}\""))
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .expect("second independent worker session should connect");

    let worker_a = PgExecutionStore::new(fixture.pool.clone());
    let worker_b = PgExecutionStore::new(second_pool.clone());

    let first_claim = worker_a
        .claim_invocation(&fixture.scope, invocation_id, "worker-a", 60)
        .await
        .expect("worker A claim")
        .expect("unleased invocation is claimable");
    assert_eq!(first_claim.fencing_token(), FencingToken::INITIAL);
    assert_eq!(first_claim.version(), 0);

    assert!(
        worker_b
            .claim_invocation(&fixture.scope, invocation_id, "worker-b", 60)
            .await
            .expect("worker B competing claim")
            .is_none(),
        "a live lease cannot be stolen"
    );

    let mut unowned_transition = first_claim.clone();
    unowned_transition
        .transition_to(InvocationStatus::ContextLoading, FencingToken::INITIAL)
        .expect("legacy candidate transition");
    assert!(
        !worker_a
            .compare_and_set_invocation(
                &fixture.scope,
                first_claim.version(),
                first_claim.fencing_token(),
                unowned_transition,
            )
            .await
            .expect("unowned write should be a CAS miss"),
        "legacy token-only writes must not bypass an active lease"
    );

    let mut context_loading = first_claim.clone();
    context_loading
        .transition_to(InvocationStatus::ContextLoading, FencingToken::INITIAL)
        .expect("worker A starts context loading");
    assert!(
        worker_a
            .compare_and_set_owned(
                &fixture.scope,
                "worker-a",
                first_claim.version(),
                first_claim.fencing_token(),
                context_loading,
            )
            .await
            .expect("worker A context transition")
    );

    let after_context = worker_a
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("reload after context")
        .expect("invocation exists");
    let mut processing = after_context.clone();
    processing
        .transition_to(InvocationStatus::Processing, FencingToken::INITIAL)
        .expect("worker A starts processing");
    assert!(
        worker_a
            .compare_and_set_owned(
                &fixture.scope,
                "worker-a",
                after_context.version(),
                after_context.fencing_token(),
                processing,
            )
            .await
            .expect("worker A processing transition")
    );

    let old_claim = worker_a
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("load stale worker snapshot")
        .expect("invocation exists");
    assert_eq!(old_claim.status(), InvocationStatus::Processing);

    sqlx::query(
        "UPDATE object_invocations SET lease_until=now()-interval '1 second' WHERE tenant_id=$1::uuid AND id=$2::uuid",
    )
    .bind(fixture.scope.tenant_id().to_string())
    .bind(invocation_id.to_string())
    .execute(&fixture.pool)
    .await
    .expect("expire worker A lease for deterministic reclaim");

    assert!(
        !worker_a
            .heartbeat_invocation(
                &fixture.scope,
                invocation_id,
                "worker-a",
                old_claim.fencing_token(),
                60,
            )
            .await
            .expect("expired heartbeat is a normal miss"),
        "an expired lease must be reclaimed, not revived"
    );

    let new_claim = worker_b
        .claim_invocation(&fixture.scope, invocation_id, "worker-b", 60)
        .await
        .expect("worker B reclaim")
        .expect("expired lease can be reclaimed");
    assert_eq!(new_claim.fencing_token().value(), 1);
    assert_eq!(new_claim.version(), old_claim.version() + 1);
    assert_eq!(new_claim.status(), InvocationStatus::Processing);

    let mut stale_output = old_claim.clone();
    stale_output
        .transition_to(InvocationStatus::Verifying, FencingToken::INITIAL)
        .expect("old in-memory state can form a candidate response transition");
    assert!(
        !worker_a
            .persist_assistant_output_owned(
                &fixture.scope,
                "worker-a",
                old_claim.version(),
                old_claim.fencing_token(),
                stale_output,
                "stale answer".to_owned(),
            )
            .await
            .expect("stale response write is a normal CAS miss"),
        "old owner and old token must not persist a response after reclaim"
    );

    assert!(
        !worker_a
            .heartbeat_invocation(
                &fixture.scope,
                invocation_id,
                "worker-a",
                FencingToken::INITIAL,
                60,
            )
            .await
            .expect("stale heartbeat is a normal miss")
    );

    let mut valid_response = new_claim.clone();
    valid_response
        .transition_to(InvocationStatus::Verifying, new_claim.fencing_token())
        .expect("new lease owner can persist provider response");
    assert!(
        worker_b
            .persist_assistant_output_owned(
                &fixture.scope,
                "worker-b",
                new_claim.version(),
                new_claim.fencing_token(),
                valid_response,
                "current answer".to_owned(),
            )
            .await
            .expect("new owner response CAS")
    );
    assert_eq!(
        worker_b
            .load_assistant_output(&fixture.scope, invocation_id)
            .await
            .expect("load assistant output")
            .as_deref(),
        Some("current answer")
    );
    assert!(
        worker_b
            .heartbeat_invocation(
                &fixture.scope,
                invocation_id,
                "worker-b",
                new_claim.fencing_token(),
                60,
            )
            .await
            .expect("new owner heartbeat")
    );

    let verifying = worker_b
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("load verifying invocation")
        .expect("invocation exists");
    let mut completed = verifying.clone();
    completed
        .transition_to(InvocationStatus::Completed, verifying.fencing_token())
        .expect("Verifying can complete");
    assert!(
        worker_b
            .complete_invocation_owned(
                &fixture.scope,
                "worker-b",
                verifying.version(),
                verifying.fencing_token(),
                completed,
            )
            .await
            .expect("terminal completion and lease release")
    );
    let terminal = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT status, lease_owner, lease_until::text FROM object_invocations WHERE tenant_id=$1::uuid AND id=$2::uuid",
    )
    .bind(fixture.scope.tenant_id().to_string())
    .bind(invocation_id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .expect("read terminal invocation");
    assert_eq!(terminal.0, "Completed");
    assert_eq!(terminal.1, None);
    assert_eq!(terminal.2, None);

    second_pool.close().await;
    teardown(fixture).await;
}

#[tokio::test]
async fn invocation_read_projection_is_tenant_scoped_and_hides_unfinished_output() {
    let fixture = setup(None).await;
    let invocation_id = match begin_invocation(
        &fixture.pool,
        &fixture.scope,
        command(&fixture, "read-projection-success"),
    )
    .await
    .expect("begin invocation")
    {
        BeginInvocationOutcome::Created { invocation_id, .. } => invocation_id,
        other => panic!("expected new invocation, got {other:?}"),
    };

    let query_store = PgInvocationQueryStore::new(fixture.pool.clone());
    let queued = query_store
        .get_invocation(&fixture.scope, invocation_id)
        .await
        .expect("read queued invocation")
        .expect("invocation exists");
    assert_eq!(queued.status(), InvocationStatus::Received);
    assert_eq!(queued.result(), None);
    assert_eq!(queued.error_code(), None);

    let other_tenant_scope = scope_for_tenant(TenantId::new());
    assert!(
        query_store
            .get_invocation(&other_tenant_scope, invocation_id)
            .await
            .expect("cross-tenant lookup is a normal miss")
            .is_none(),
        "an invocation from another tenant must be indistinguishable from a missing ID"
    );

    let store = PgExecutionStore::new(fixture.pool.clone());
    let received = store
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("load invocation")
        .expect("invocation exists");
    let mut context_loading = received.clone();
    context_loading
        .transition_to(InvocationStatus::ContextLoading, FencingToken::INITIAL)
        .expect("Received -> ContextLoading");
    assert!(
        store
            .compare_and_set_invocation(
                &fixture.scope,
                received.version(),
                FencingToken::INITIAL,
                context_loading,
            )
            .await
            .expect("persist context loading")
    );

    let context = store
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("reload invocation")
        .expect("invocation exists");
    let mut processing = context.clone();
    processing
        .transition_to(InvocationStatus::Processing, FencingToken::INITIAL)
        .expect("ContextLoading -> Processing");
    assert!(
        store
            .compare_and_set_invocation(
                &fixture.scope,
                context.version(),
                FencingToken::INITIAL,
                processing,
            )
            .await
            .expect("persist processing")
    );

    let processing = store
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("reload processing invocation")
        .expect("invocation exists");
    let mut verifying = processing.clone();
    verifying
        .transition_to(InvocationStatus::Verifying, FencingToken::INITIAL)
        .expect("Processing -> Verifying");
    assert!(
        store
            .persist_assistant_output(
                &fixture.scope,
                processing.version(),
                FencingToken::INITIAL,
                verifying,
                "private until completed".to_owned(),
            )
            .await
            .expect("persist assistant output")
    );

    let running = query_store
        .get_invocation(&fixture.scope, invocation_id)
        .await
        .expect("read verifying invocation")
        .expect("invocation exists");
    assert_eq!(running.status(), InvocationStatus::Verifying);
    assert_eq!(
        running.result(),
        None,
        "unfinished assistant output is not exposed"
    );

    let verifying = store
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("reload verifying invocation")
        .expect("invocation exists");
    let mut completed = verifying.clone();
    completed
        .transition_to(InvocationStatus::Completed, FencingToken::INITIAL)
        .expect("Verifying -> Completed");
    assert!(
        store
            .complete_invocation(
                &fixture.scope,
                verifying.version(),
                FencingToken::INITIAL,
                completed,
            )
            .await
            .expect("complete invocation")
    );

    let succeeded = query_store
        .get_invocation(&fixture.scope, invocation_id)
        .await
        .expect("read completed invocation")
        .expect("invocation exists");
    assert_eq!(succeeded.status(), InvocationStatus::Completed);
    assert_eq!(succeeded.result(), Some("private until completed"));
    assert_eq!(succeeded.error_code(), None);

    let failed_id = match begin_invocation(
        &fixture.pool,
        &fixture.scope,
        command(&fixture, "read-projection-failed"),
    )
    .await
    .expect("begin failure case")
    {
        BeginInvocationOutcome::Created { invocation_id, .. } => invocation_id,
        other => panic!("expected new invocation, got {other:?}"),
    };
    let failed_current = store
        .load_invocation(&fixture.scope, failed_id)
        .await
        .expect("load failure case")
        .expect("invocation exists");
    let mut failed = failed_current.clone();
    failed
        .transition_to(InvocationStatus::Failed, FencingToken::INITIAL)
        .expect("Received -> Failed");
    assert!(
        store
            .fail_invocation(
                &fixture.scope,
                failed_current.version(),
                FencingToken::INITIAL,
                failed,
                "provider_timeout",
            )
            .await
            .expect("persist failure")
    );

    let failure_view = query_store
        .get_invocation(&fixture.scope, failed_id)
        .await
        .expect("read failed invocation")
        .expect("invocation exists");
    assert_eq!(failure_view.status(), InvocationStatus::Failed);
    assert_eq!(failure_view.result(), None);
    assert_eq!(failure_view.error_code(), Some("provider_timeout"));

    teardown(fixture).await;
}

#[tokio::test]
async fn invocation_cancellation_is_idempotent_and_fences_an_active_worker() {
    let fixture = setup(None).await;
    let invocation_id = match begin_invocation(
        &fixture.pool,
        &fixture.scope,
        command(&fixture, "cancel-active-worker"),
    )
    .await
    .expect("begin invocation")
    {
        BeginInvocationOutcome::Created { invocation_id, .. } => invocation_id,
        other => panic!("expected new invocation, got {other:?}"),
    };

    let store = PgExecutionStore::new(fixture.pool.clone());
    let claim = store
        .claim_invocation(&fixture.scope, invocation_id, "worker-cancel", 60)
        .await
        .expect("claim invocation")
        .expect("invocation is claimable");
    let outcome = store
        .cancel_invocation(&fixture.scope, invocation_id)
        .await
        .expect("cancel invocation");
    assert_eq!(
        outcome,
        InvocationCancellationOutcome::Cancelled {
            newly_cancelled: true
        }
    );

    let row = sqlx::query_as::<_, (String, Option<String>, Option<String>, i64, i64)>(
        "SELECT status, cancel_requested_at::text, lease_owner, fencing_token, version FROM object_invocations WHERE tenant_id=$1::uuid AND id=$2::uuid",
    )
    .bind(fixture.scope.tenant_id().to_string())
    .bind(invocation_id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .expect("read cancelled invocation");
    assert_eq!(row.0, "Cancelled");
    assert!(row.1.is_some(), "cancellation timestamp is durable");
    assert_eq!(row.2, None, "cancellation releases the lease");
    assert_eq!(row.3, 1, "cancellation advances the fencing token");
    assert!(row.4 > i64::try_from(claim.version()).expect("version fits in bigint"));

    assert!(
        !store
            .heartbeat_invocation(
                &fixture.scope,
                invocation_id,
                "worker-cancel",
                claim.fencing_token(),
                60,
            )
            .await
            .expect("stale heartbeat is a normal miss"),
        "cancelled worker must no longer renew its lease"
    );

    let mut stale_transition = claim.clone();
    stale_transition
        .transition_to(InvocationStatus::ContextLoading, claim.fencing_token())
        .expect("construct stale candidate transition");
    assert!(
        !store
            .compare_and_set_owned(
                &fixture.scope,
                "worker-cancel",
                claim.version(),
                claim.fencing_token(),
                stale_transition,
            )
            .await
            .expect("stale write is a normal CAS miss"),
        "cancelled worker must not persist a lifecycle transition"
    );

    let repeated = store
        .cancel_invocation(&fixture.scope, invocation_id)
        .await
        .expect("repeat cancellation");
    assert_eq!(
        repeated,
        InvocationCancellationOutcome::Cancelled {
            newly_cancelled: false
        }
    );

    let missing = store
        .cancel_invocation(&fixture.scope, InvocationId::new())
        .await
        .expect("unknown invocation is a normal miss");
    assert_eq!(missing, InvocationCancellationOutcome::NotFound);

    let other_tenant = scope_for_tenant(TenantId::new());
    assert_eq!(
        store
            .cancel_invocation(&other_tenant, invocation_id)
            .await
            .expect("cross-tenant lookup is a normal miss"),
        InvocationCancellationOutcome::NotFound
    );

    teardown(fixture).await;
}

#[tokio::test]
async fn invocation_cancellation_does_not_rewrite_another_terminal_state() {
    let fixture = setup(None).await;
    let invocation_id = match begin_invocation(
        &fixture.pool,
        &fixture.scope,
        command(&fixture, "cancel-terminal"),
    )
    .await
    .expect("begin invocation")
    {
        BeginInvocationOutcome::Created { invocation_id, .. } => invocation_id,
        other => panic!("expected new invocation, got {other:?}"),
    };

    let store = PgExecutionStore::new(fixture.pool.clone());
    let current = store
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("load invocation")
        .expect("invocation exists");
    let mut failed = current.clone();
    failed
        .transition_to(InvocationStatus::Failed, FencingToken::INITIAL)
        .expect("Received -> Failed");
    assert!(
        store
            .fail_invocation(
                &fixture.scope,
                current.version(),
                FencingToken::INITIAL,
                failed,
                "provider_timeout",
            )
            .await
            .expect("persist terminal failure")
    );

    let outcome = store
        .cancel_invocation(&fixture.scope, invocation_id)
        .await
        .expect("cancel terminal invocation");
    assert_eq!(
        outcome,
        InvocationCancellationOutcome::AlreadyTerminal(InvocationStatus::Failed)
    );

    let row = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, cancel_requested_at::text FROM object_invocations WHERE tenant_id=$1::uuid AND id=$2::uuid",
    )
    .bind(fixture.scope.tenant_id().to_string())
    .bind(invocation_id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .expect("read failed invocation");
    assert_eq!(row.0, "Failed");
    assert_eq!(row.1, None, "terminal failure is not marked cancelled");

    teardown(fixture).await;
}


#[tokio::test]
async fn object_daily_limit_is_aggregate_across_pools_and_ignores_anonymous_share() {
    let fixture = setup(None).await;
    let estimate = qembed_domain::budget::estimate_input_tokens(
        command(&fixture, "daily-auth").input_message.len(),
    ).expect("input estimate");
    sqlx::query("UPDATE object_budgets SET daily_token_limit=$2, anonymous_share_pct=0 WHERE object_id=$1::uuid")
        .bind(fixture.object_id.to_string()).bind(i64::try_from(estimate).expect("estimate fits bigint"))
        .execute(&fixture.pool).await.expect("aggregate daily limit should be configured");

    let first = begin_invocation(&fixture.pool, &fixture.scope, command(&fixture, "daily-auth"))
        .await.expect("authenticated reservation should fit the aggregate daily limit");
    assert!(matches!(first, BeginInvocationOutcome::Created { .. }));

    let mut anonymous = command(&fixture, "daily-anon");
    anonymous.invocation_pool = InvocationPool::Anonymous;
    let error = begin_invocation(&fixture.pool, &fixture.scope, anonymous)
        .await.expect_err("anonymous pool must share the aggregate object ceiling");
    assert!(matches!(error, BeginInvocationError::Budget(BudgetReservationError::Rejected(
        BudgetRejection::ObjectDailyLimit
    ))));
    let pool_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_counters")
        .fetch_one(&fixture.pool).await.expect("pool counter rows");
    assert_eq!(pool_rows, 1, "only the first pool has a committed reservation");
    assert_eq!(counts(&fixture).await, (1, 1, 1, 1, 1));
    teardown(fixture).await;
}

#[tokio::test]
async fn caller_daily_limit_rejects_and_rolls_back_all_begin_rows() {
    let fixture = setup(None).await;
    sqlx::query("UPDATE object_budgets SET per_caller_daily_token_limit=1 WHERE object_id=$1::uuid")
        .bind(fixture.object_id.to_string()).execute(&fixture.pool).await
        .expect("caller daily limit should be configured");
    let error = begin_invocation(&fixture.pool, &fixture.scope, command(&fixture, "caller-limit"))
        .await.expect_err("caller limit must reject a non-zero estimate");
    assert!(matches!(error, BeginInvocationError::Budget(BudgetReservationError::Rejected(
        BudgetRejection::CallerDailyLimit
    ))));
    assert_eq!(counts(&fixture).await, (0, 0, 0, 0, 0));
    let reservations: i64 = sqlx::query_scalar("SELECT count(*) FROM budget_reservations")
        .fetch_one(&fixture.pool).await.expect("reservation count");
    assert_eq!(reservations, 0);
    teardown(fixture).await;
}

#[tokio::test]
async fn concurrent_invocation_limit_counts_active_reservations() {
    let fixture = setup(None).await;
    sqlx::query("UPDATE object_budgets SET max_concurrent_invocations=1 WHERE object_id=$1::uuid")
        .bind(fixture.object_id.to_string()).execute(&fixture.pool).await
        .expect("concurrency limit should be configured");
    let first = begin_invocation(&fixture.pool, &fixture.scope, command(&fixture, "concurrent-1"))
        .await.expect("first invocation should reserve the only slot");
    assert!(matches!(first, BeginInvocationOutcome::Created { .. }));
    let error = begin_invocation(&fixture.pool, &fixture.scope, command(&fixture, "concurrent-2"))
        .await.expect_err("second active invocation must be rejected");
    assert!(matches!(error, BeginInvocationError::Budget(BudgetReservationError::Rejected(
        BudgetRejection::ConcurrentInvocationLimit
    ))));
    assert_eq!(counts(&fixture).await, (1, 1, 1, 1, 1));
    teardown(fixture).await;
}

#[tokio::test]
async fn competing_sessions_cannot_overspend_the_object_daily_budget() {
    let fixture = setup(None).await;
    let estimate = qembed_domain::budget::estimate_input_tokens(
        command(&fixture, "race-a").input_message.len(),
    ).expect("input estimate");
    sqlx::query("UPDATE object_budgets SET daily_token_limit=$2 WHERE object_id=$1::uuid")
        .bind(fixture.object_id.to_string()).bind(i64::try_from(estimate).expect("estimate fits bigint"))
        .execute(&fixture.pool).await.expect("daily budget should be configured");

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let schema = fixture.schema.clone();
    let second_pool = PgPoolOptions::new().max_connections(1).after_connect(move |connection, _| {
        let schema = schema.clone();
        Box::pin(async move {
            sqlx::query(&format!("SET search_path TO \"{schema}\""))
                .execute(connection).await?;
            Ok(())
        })
    }).connect(&database_url).await.expect("second independent pool");

    let first_command = command(&fixture, "race-a");
    let second_command = command(&fixture, "race-b");
    let (first, second) = tokio::join!(
        begin_invocation(&fixture.pool, &fixture.scope, first_command),
        begin_invocation(&second_pool, &fixture.scope, second_command),
    );
    second_pool.close().await;

    let first_created = matches!(first.as_ref(), Ok(BeginInvocationOutcome::Created { .. }));
    let second_created = matches!(second.as_ref(), Ok(BeginInvocationOutcome::Created { .. }));
    let successes = first_created as usize + second_created as usize;
    assert_eq!(successes, 1, "exactly one independent session may consume the final budget");
    let rejected = match (&first, &second) {
        (Err(BeginInvocationError::Budget(BudgetReservationError::Rejected(BudgetRejection::ObjectDailyLimit))), _)
        | (_, Err(BeginInvocationError::Budget(BudgetReservationError::Rejected(BudgetRejection::ObjectDailyLimit)))) => true,
        _ => false,
    };
    assert!(rejected, "the losing session must observe the committed reservation");
    let reserved: i64 = sqlx::query_scalar("SELECT sum(reserved_tokens) FROM usage_counters")
        .fetch_one(&fixture.pool).await.expect("reserved token sum");
    assert_eq!(reserved, i64::try_from(estimate).expect("estimate fits bigint"));
    teardown(fixture).await;
}
