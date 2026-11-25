mod common;

use qembed_domain::{
    ids::{CallerId, ObjectId, TenantId},
    test_support::scope_for_tenant,
};
use qembed_store::episodic_memory::{EpisodicMemoryError, PgEpisodicMemoryRepository};
use uuid::Uuid;

async fn seed_caller(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    principal_id: Uuid,
    caller_id: CallerId,
    token_byte: u8,
    delegated: bool,
) {
    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind) \
         VALUES ($1::uuid, $2::uuid, $3, 'Authenticated')",
    )
    .bind(caller_id.to_string())
    .bind(tenant_id.to_string())
    .bind(vec![token_byte; 32])
    .execute(pool)
    .await
    .expect("Caller fixture inserted");

    if delegated {
        sqlx::query(
            "INSERT INTO principal_caller_delegations \
             (delegation_id, tenant_id, principal_id, caller_id) \
             VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid)",
        )
        .bind(Uuid::new_v4())
        .bind(tenant_id.to_string())
        .bind(principal_id.to_string())
        .bind(caller_id.to_string())
        .execute(pool)
        .await
        .expect("direct delegation fixture inserted");
    }
}

#[tokio::test]
#[ignore = "requires canonical qembed-sql RT-050 signed-context migration; run from scripts/test-postgres.sh"]
async fn caller_scoped_application_path_cannot_read_another_callers_episodic_memory() {
    let pool = common::connect().await;
    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let principal_id = scope.principal().principal_id().into_uuid();
    let caller_a = CallerId::new();
    let caller_b = CallerId::new();
    let caller_without_delegation = CallerId::new();
    let object_id = ObjectId::new();

    sqlx::query("INSERT INTO tenants (id) VALUES ($1::uuid)")
        .bind(tenant_id.to_string())
        .execute(&pool)
        .await
        .expect("tenant fixture inserted");
    sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) \
         VALUES ($1::uuid, $2::uuid, 'tenant_owner')",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .execute(&pool)
    .await
    .expect("Owner Principal fixture inserted");

    seed_caller(&pool, tenant_id, principal_id, caller_a, 11, true).await;
    seed_caller(&pool, tenant_id, principal_id, caller_b, 22, true).await;
    seed_caller(
        &pool,
        tenant_id,
        principal_id,
        caller_without_delegation,
        33,
        false,
    )
    .await;

    sqlx::query(
        "INSERT INTO objects (tenant_id, object_id, state) \
         VALUES ($1::uuid, $2::uuid, 'draft')",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(&pool)
    .await
    .expect("Object fixture inserted");

    let caller_a_memory_id = Uuid::new_v4();
    let caller_b_memory_id = Uuid::new_v4();
    for (memory_id, caller_id, claim) in [
        (caller_a_memory_id, caller_a, "Caller A private memory"),
        (caller_b_memory_id, caller_b, "Caller B secret"),
    ] {
        sqlx::query(
            "INSERT INTO memories \
             (id, tenant_id, object_id, caller_id, memory_kind, claim, source_kind, verification, confidence, expires_at) \
             VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 'episodic', $5, 'User', 'Unverified', 0.5, clock_timestamp() + interval '1 day')",
        )
        .bind(memory_id)
        .bind(tenant_id.to_string())
        .bind(object_id.to_string())
        .bind(caller_id.to_string())
        .bind(claim)
        .execute(&pool)
        .await
        .expect("episodic memory fixture inserted");
    }

    let context = common::signed_context(&pool).await;
    let repository = PgEpisodicMemoryRepository::new(context.clone());

    // First prove that the signed qctx Caller claim independently filters the
    // canonical RLS policy, even without an explicit caller_id SQL predicate.
    let (mut transaction, resolved) = context
        .begin_caller(&scope, caller_a)
        .await
        .expect("Caller A delegation should resolve");
    assert_eq!(resolved.caller_id(), caller_a);
    let rls_visible_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memories \
          WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND memory_kind = 'episodic'",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .fetch_one(transaction.connection())
    .await
    .expect("signed Caller context should apply the canonical RLS policy");
    assert_eq!(rls_visible_count, 1);
    transaction
        .commit()
        .await
        .expect("Caller A context transaction should commit");

    let visible_to_a = repository
        .list_episodic(&scope, caller_a, object_id)
        .await
        .expect("Caller A can list its own memories");
    assert_eq!(visible_to_a.len(), 1);
    assert_eq!(visible_to_a[0].claim(), "Caller A private memory");
    assert_eq!(visible_to_a[0].source_kind(), "User");
    assert_eq!(visible_to_a[0].verification(), "Unverified");
    assert_eq!(visible_to_a[0].id(), caller_a_memory_id);
    assert_ne!(visible_to_a[0].id(), caller_b_memory_id);

    let visible_to_b = repository
        .list_episodic(&scope, caller_b, object_id)
        .await
        .expect("Caller B can list its own memories");
    assert_eq!(visible_to_b.len(), 1);
    assert_eq!(visible_to_b[0].claim(), "Caller B secret");
    assert_eq!(visible_to_b[0].id(), caller_b_memory_id);
    assert_ne!(visible_to_b[0].id(), caller_a_memory_id);

    assert!(matches!(
        repository
            .list_episodic(&scope, caller_without_delegation, object_id)
            .await,
        Err(EpisodicMemoryError::Context(_))
    ));

    pool.close().await;
}
