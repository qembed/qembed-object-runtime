mod common;

use qembed_domain::{
    auth::authenticate_principal,
    ids::{CallerId, PrincipalId, TenantId},
};
use qembed_store::{
    auth::{PrincipalAuthenticator, PrincipalCredentials},
    caller_delegation::{CallerDelegationError, PgCallerDelegationRepository},
    run_migrations,
};
use sha2::{Digest, Sha256};

async fn seed_principal(
    pool: &sqlx::PgPool,
    tenant: TenantId,
    id: PrincipalId,
    token: &str,
    kind: &str,
) {
    sqlx::query("INSERT INTO principals (id, tenant_id, kind, key_hash) VALUES ($1::uuid, $2::uuid, $3, $4)")
        .bind(id.to_string())
        .bind(tenant.to_string())
        .bind(kind)
        .bind(Sha256::digest(token.as_bytes()).to_vec())
        .execute(pool)
        .await
        .expect("Principal fixture should be inserted");
}

#[tokio::test]
async fn owner_can_delegate_and_revoke_caller_for_same_tenant_principal() {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should apply");

    let tenant = TenantId::new();
    let other_tenant = TenantId::new();
    let owner_id = PrincipalId::new();
    let admin_id = PrincipalId::new();
    let other_admin_id = PrincipalId::new();
    let owner_token = format!("owner-{}", PrincipalId::new());
    let admin_token = format!("admin-{}", PrincipalId::new());
    let other_token = format!("other-{}", PrincipalId::new());
    let caller_id = CallerId::new();
    let caller_hash = Sha256::digest(format!("caller-{caller_id}").as_bytes()).to_vec();

    sqlx::query("INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) VALUES ($1::uuid, $2::uuid, 'tenant_owner')")
        .bind(tenant.to_string())
        .bind(owner_id.to_string())
        .execute(&pool)
        .await
        .expect("legacy Owner fixture should be inserted and mirrored to principals");
    sqlx::query("UPDATE principals SET key_hash = $1 WHERE tenant_id = $2::uuid AND id = $3::uuid")
        .bind(Sha256::digest(owner_token.as_bytes()).to_vec())
        .bind(tenant.to_string())
        .bind(owner_id.to_string())
        .execute(&pool)
        .await
        .expect("Owner Principal token hash should be configured");
    seed_principal(&pool, tenant, admin_id, &admin_token, "TenantAdmin").await;
    seed_principal(
        &pool,
        other_tenant,
        other_admin_id,
        &other_token,
        "TenantAdmin",
    )
    .await;
    sqlx::query("INSERT INTO callers (id, tenant_id, token_hash, caller_kind) VALUES ($1::uuid, $2::uuid, $3, 'Authenticated')")
        .bind(caller_id.to_string())
        .bind(tenant.to_string())
        .bind(caller_hash)
        .execute(&pool)
        .await
        .expect("Caller fixture should be inserted");

    let context = common::signed_context(&pool).await;
    let authenticator = PrincipalAuthenticator::new(pool.clone());
    let owner_scope =
        authenticate_principal(&authenticator, PrincipalCredentials::new(owner_token))
            .await
            .expect("Owner token should authenticate");
    let admin_scope =
        authenticate_principal(&authenticator, PrincipalCredentials::new(admin_token))
            .await
            .expect("TenantAdmin token should authenticate");
    let repository = PgCallerDelegationRepository::new(context.clone());

    let delegation_id = repository
        .grant(&owner_scope, admin_id, caller_id)
        .await
        .expect("Owner should grant a Caller to another same-tenant Principal");

    let (resolved_transaction, resolved_caller) = context
        .begin_caller(&admin_scope, caller_id)
        .await
        .expect("delegated TenantAdmin should resolve its Caller");
    assert_eq!(resolved_caller.kind(), CallerKind::Authenticated);
    resolved_transaction
        .commit()
        .await
        .expect("Caller resolution transaction should commit");

    assert!(matches!(
        repository.grant(&owner_scope, admin_id, caller_id).await,
        Err(CallerDelegationError::AlreadyActive)
    ));

    assert!(matches!(
        repository.grant(&admin_scope, admin_id, caller_id).await,
        Err(CallerDelegationError::NotOwner)
    ));
    assert!(matches!(
        repository
            .grant(&owner_scope, other_admin_id, caller_id)
            .await,
        Err(CallerDelegationError::NotFound)
    ));

    repository
        .revoke(&owner_scope, delegation_id)
        .await
        .expect("Owner should revoke the active delegation");
    assert!(
        context
            .begin_caller(&admin_scope, caller_id)
            .await
            .is_err(),
        "revoked delegation must no longer resolve"
    );
    assert!(matches!(
        repository.revoke(&owner_scope, delegation_id).await,
        Err(CallerDelegationError::NotFound)
    ));

    let history: (Option<time::OffsetDateTime>, Option<String>) =
        sqlx::query_as("SELECT revoked_at, revoked_by_principal_id::text FROM principal_caller_delegations WHERE delegation_id = $1::uuid")
            .bind(delegation_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("revoked delegation history should remain");
    assert!(history.0.is_some());
    let owner_id_string = owner_id.to_string();
    assert_eq!(history.1.as_deref(), Some(owner_id_string.as_str()));

    pool.close().await;
}

#[tokio::test]
async fn legacy_owner_sync_rejects_collision_with_unrelated_principal() {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should apply");

    let tenant = TenantId::new();
    let collision_id = PrincipalId::new();
    let token = format!("tenant-admin-{}", PrincipalId::new());
    seed_principal(&pool, tenant, collision_id, &token, "TenantAdmin").await;

    let error = sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) VALUES ($1::uuid, $2::uuid, 'tenant_owner')",
    )
    .bind(tenant.to_string())
    .bind(collision_id.to_string())
    .execute(&pool)
    .await
    .expect_err("legacy Owner ID must not alias a non-Owner Principal");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.code())
            .as_deref(),
        Some("23505")
    );
    pool.close().await;
}
