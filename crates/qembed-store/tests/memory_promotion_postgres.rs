mod common;

use qembed_domain::{
    ids::{ObjectId, TenantId},
    test_support::{scope_for_authenticated_tenant, scope_for_tenant},
};
use qembed_store::memory_promotion::{MemoryPromotionError, PgMemoryPromotionRepository};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires canonical qembed-sql schema and RT-050 signed-context migration; run from scripts/test-postgres.sh"]
async fn owner_promotion_creates_new_verified_row_and_preserves_candidate() {
    let pool = common::connect().await;
    let tenant_id = TenantId::new();
    let object_id = ObjectId::new();
    let scope = scope_for_tenant(tenant_id);
    let principal_id = scope.principal().principal_id().into_uuid();
    let candidate_id = Uuid::new_v4();

    sqlx::query("INSERT INTO tenants (id) VALUES ($1::uuid)")
        .bind(tenant_id.to_string())
        .execute(&pool)
        .await
        .expect("tenant fixture");

    sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) \
         VALUES ($1::uuid, $2::uuid, 'tenant_owner')",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .execute(&pool)
    .await
    .expect("Owner principal fixture");

    sqlx::query(
        "INSERT INTO objects (tenant_id, object_id, state) VALUES ($1::uuid, $2::uuid, 'draft')",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(&pool)
    .await
    .expect("Object fixture");

    sqlx::query(
        "INSERT INTO memories (id, tenant_id, object_id, memory_kind, claim, source_kind, verification, confidence) \
         VALUES ($1, $2::uuid, $3::uuid, 'semantic', 'User supplied fact', 'User', 'Unverified', 0.4)",
    )
    .bind(candidate_id)
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(&pool)
    .await
    .expect("candidate fixture");

    let repository = PgMemoryPromotionRepository::new(common::signed_context(&pool).await);
    let promoted = repository
        .promote(&scope, object_id, candidate_id)
        .await
        .expect("Owner promotion succeeds");

    assert_ne!(promoted.id(), candidate_id);
    assert_eq!(promoted.candidate_id(), candidate_id);
    assert_eq!(promoted.claim(), "User supplied fact");

    let candidate: (String, String, String, Option<String>, Option<String>, bool) = sqlx::query_as(
        "SELECT claim, source_kind, verification, verified_by::text, supersedes_id::text, verified_at IS NULL \
           FROM memories WHERE id = $1",
    )
    .bind(candidate_id)
    .fetch_one(&pool)
    .await
    .expect("candidate remains persisted");
    assert_eq!(
        candidate,
        (
            "User supplied fact".to_owned(),
            "User".to_owned(),
            "Unverified".to_owned(),
            None,
            None,
            true
        )
    );

    let verified: (String, String, String, f32, Option<String>, Option<String>, bool) =
        sqlx::query_as(
            "SELECT claim, source_kind, verification, confidence, supersedes_id::text, verified_by::text, verified_at IS NOT NULL \
               FROM memories WHERE id = $1",
        )
        .bind(promoted.id())
        .fetch_one(&pool)
        .await
        .expect("new Verified row persisted");
    assert_eq!(verified.0, "User supplied fact");
    assert_eq!(verified.1, "Owner");
    assert_eq!(verified.2, "Verified");
    assert_eq!(verified.3, 1.0);
    assert!(verified.6);

    let candidate_id_text = candidate_id.to_string();
    let principal_id_text = principal_id.to_string();
    assert_eq!(verified.4.as_deref(), Some(candidate_id_text.as_str()));
    assert_eq!(verified.5.as_deref(), Some(principal_id_text.as_str()));

    assert!(matches!(
        repository.promote(&scope, object_id, candidate_id).await,
        Err(MemoryPromotionError::AlreadyPromoted)
    ));

    let non_owner_scope = scope_for_authenticated_tenant(tenant_id);
    assert!(matches!(
        repository
            .promote(&non_owner_scope, object_id, candidate_id)
            .await,
        Err(MemoryPromotionError::NotTenantOwner)
    ));

    pool.close().await;
}
