mod common;

use qembed_domain::{
    ids::{CallerId, ObjectId, PrincipalId, TenantId},
    test_support::scope_for_tenant,
};
use qembed_store::{
    episodic_memory::{EpisodicMemoryError, PgEpisodicMemoryRepository},
    run_migrations,
};

async fn seed_caller(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    principal_id: PrincipalId,
    caller_id: CallerId,
) {
    sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) \
         VALUES ($1::uuid, $2::uuid, 'tenant_owner')",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .execute(pool)
    .await
    .expect("Principal fixture inserted");

    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind) \
         VALUES ($1::uuid, $2::uuid, $3, 'Authenticated')",
    )
    .bind(caller_id.to_string())
    .bind(tenant_id.to_string())
    .bind(vec![11_u8; 32])
    .execute(pool)
    .await
    .expect("Caller fixture inserted");

    sqlx::query(
        "INSERT INTO principal_caller_delegations \
         (delegation_id, tenant_id, principal_id, caller_id) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid)",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .bind(caller_id.to_string())
    .execute(pool)
    .await
    .expect("direct delegation fixture inserted");
}

#[tokio::test]
async fn caller_scoped_application_path_cannot_read_another_callers_episodic_memory() {
    let pool = common::connect().await;
    run_migrations(&pool).await.expect("migrations apply");

    let tenant_id = TenantId::new();
    sqlx::query("INSERT INTO tenants (id) VALUES ($1::uuid)")
        .bind(tenant_id.to_string())
        .execute(&pool)
        .await
        .expect("tenant fixture inserted");

    let scope_a = scope_for_tenant(tenant_id);
    let caller_a = CallerId::new();
    let caller_b = CallerId::new();
    seed_caller(
        &pool,
        tenant_id,
        scope_a.principal().principal_id(),
        caller_a,
    )
    .await;

    // Caller B exists in the same tenant and Object, but is not delegated to
    // Principal A. Its private row is seeded outside the application path.
    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind) \
         VALUES ($1::uuid, $2::uuid, $3, 'Authenticated')",
    )
    .bind(caller_b.to_string())
    .bind(tenant_id.to_string())
    .bind(vec![22_u8; 32])
    .execute(&pool)
    .await
    .expect("Caller B fixture inserted");

    let object_id = ObjectId::new();
    sqlx::query(
        "INSERT INTO objects (tenant_id, object_id, state) \
         VALUES ($1::uuid, $2::uuid, 'draft')",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(&pool)
    .await
    .expect("Object fixture inserted");

    let caller_a_memory_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO memories \
         (id, tenant_id, object_id, caller_id, memory_kind, claim, source_kind, verification, confidence, expires_at) \
         VALUES (gen_random_uuid(), $1::uuid, $2::uuid, $3::uuid, 'episodic', 'Caller A private memory', 'User', 'Unverified', 0.5, clock_timestamp() + interval '1 day') \
         RETURNING id",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(caller_a.to_string())
    .fetch_one(&pool)
    .await
    .expect("Caller A memory fixture inserted");
    let caller_b_memory_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO memories \
         (id, tenant_id, object_id, caller_id, memory_kind, claim, source_kind, verification, confidence, expires_at) \
         VALUES (gen_random_uuid(), $1::uuid, $2::uuid, $3::uuid, 'episodic', 'Caller B secret', 'User', 'Unverified', 0.5, clock_timestamp() + interval '1 day') \
         RETURNING id",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(caller_b.to_string())
    .fetch_one(&pool)
    .await
    .expect("Caller B memory fixture inserted");

    let context = common::signed_context(&pool).await;
    let repository = PgEpisodicMemoryRepository::new(context);

    let visible_to_a = repository
        .list_episodic(&scope_a, caller_a, object_id)
        .await
        .expect("Caller A can list its own memories");
    assert_eq!(visible_to_a.len(), 1);
    assert_eq!(visible_to_a[0].claim(), "Caller A private memory");
    assert_eq!(visible_to_a[0].source_kind(), "User");
    assert_eq!(visible_to_a[0].verification(), "Unverified");
    assert_eq!(visible_to_a[0].id(), caller_a_memory_id);
    assert_ne!(visible_to_a[0].id(), caller_b_memory_id);

    assert!(matches!(
        repository.list_episodic(&scope_a, caller_b, object_id).await,
        Err(EpisodicMemoryError::Context(_))
    ));

    pool.close().await;
}
