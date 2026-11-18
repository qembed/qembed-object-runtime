mod common;

use std::time::{Duration, SystemTime};

use qembed_domain::caller::InvocationPool;
use qembed_domain::ids::{CallerId, ObjectId, ObjectVersionId, SnapshotId, TenantId, TraceId};
use qembed_domain::invocation::{FencingToken, InvocationStatus};
use qembed_domain::ports::{ExecutionStore, InvocationWorkStore};
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::begin_invocation::{
    BeginInvocationCommand, BeginInvocationError, BeginInvocationOutcome, begin_invocation,
};
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
            "CREATE TABLE \"{schema}\".object_invocations (id UUID PRIMARY KEY, tenant_id UUID NOT NULL, object_id UUID NOT NULL, caller_id UUID NOT NULL, snapshot_id UUID NOT NULL, knowledge_revision BIGINT NOT NULL CHECK (knowledge_revision >= 0), pool TEXT NOT NULL CHECK (pool IN ('anonymous','authenticated')), status TEXT NOT NULL CHECK (status IN ('Received','ContextLoading','Processing','ExecutingTool','Verifying','WaitingForInput','WaitingForApproval','Completed','CompletedDegraded','Failed','Cancelled')), trace_id UUID NOT NULL, fencing_token BIGINT NOT NULL DEFAULT 0 CHECK (fencing_token >= 0), started_at TIMESTAMPTZ NOT NULL DEFAULT now(), finished_at TIMESTAMPTZ, version BIGINT NOT NULL DEFAULT 0 CHECK (version >= 0), UNIQUE (tenant_id, object_id, id), UNIQUE (tenant_id, object_id, caller_id, id), UNIQUE (tenant_id, object_id, id, pool), FOREIGN KEY (tenant_id, object_id) REFERENCES \"{schema}\".objects(tenant_id, object_id), FOREIGN KEY (tenant_id, caller_id) REFERENCES \"{schema}\".callers(tenant_id, id), FOREIGN KEY (tenant_id, object_id, snapshot_id) REFERENCES \"{schema}\".snapshots(tenant_id, object_id, snapshot_id))"
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
    sqlx::query("INSERT INTO object_versions (tenant_id, object_id, object_version_id, version_number, status) VALUES ($1::uuid, $2::uuid, $3::uuid, 1, 'published')")
        .bind(tenant_id.to_string()).bind(object_id.to_string()).bind(version_id.to_string())
        .execute(&pool).await.expect("published version fixture should be inserted");
    sqlx::query("INSERT INTO snapshots (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, '{}'::bytea, decode(repeat('00',32),'hex'), '{}'::jsonb)")
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
    assert!(store
        .compare_and_set_invocation(
            &fixture.scope,
            current.version(),
            FencingToken::INITIAL,
            next,
        )
        .await
        .expect("persist context-loading state"));

    current = store
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("reload invocation")
        .expect("invocation exists");
    let mut next = current.clone();
    next.transition_to(InvocationStatus::Processing, FencingToken::INITIAL)
        .expect("ContextLoading -> Processing");
    assert!(store
        .compare_and_set_invocation(
            &fixture.scope,
            current.version(),
            FencingToken::INITIAL,
            next,
        )
        .await
        .expect("persist processing state"));

    current = store
        .load_invocation(&fixture.scope, invocation_id)
        .await
        .expect("reload processing invocation")
        .expect("invocation exists");
    let mut next = current.clone();
    next.transition_to(InvocationStatus::Verifying, FencingToken::INITIAL)
        .expect("Processing -> Verifying");
    assert!(store
        .persist_assistant_output(
            &fixture.scope,
            current.version(),
            FencingToken::INITIAL,
            next,
            "durable answer".to_owned(),
        )
        .await
        .expect("persist response and verifying state"));

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
    assert!(restarted
        .complete_invocation(
            &fixture.scope,
            recovered.version(),
            FencingToken::INITIAL,
            completed,
        )
        .await
        .expect("persist terminal state"));

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
