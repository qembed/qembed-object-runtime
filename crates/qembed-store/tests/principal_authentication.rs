mod common;

use qembed_domain::{
    auth::{PrincipalAuthenticationError as DomainAuthError, authenticate_principal},
    ids::{PrincipalId, TenantId},
    principal::PrincipalKind,
};
use qembed_store::{
    auth::{
        PrincipalAuthenticationError as StoreAuthError, PrincipalAuthenticator,
        PrincipalCredentials,
    },
    run_migrations,
};
use sha2::{Digest, Sha256};

async fn seed_principal(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    principal_id: PrincipalId,
    kind: &str,
    token: &str,
) {
    sqlx::query(
        "INSERT INTO principals (id, tenant_id, kind, key_hash) VALUES ($1::uuid, $2::uuid, $3, $4)",
    )
    .bind(principal_id.to_string())
    .bind(tenant_id.to_string())
    .bind(kind)
    .bind(Sha256::digest(token.as_bytes()).to_vec())
    .execute(pool)
    .await
    .expect("Principal fixture should be inserted");
}

#[tokio::test]
async fn canonical_principal_authentication_maps_tenant_roles_and_rejects_platform_admin() {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should be applied");

    let tenant_id = TenantId::new();
    let tenant_admin_id = PrincipalId::new();
    let service_id = PrincipalId::new();
    let owner_id = PrincipalId::new();
    let admin_token = format!("tenant-admin-{}", PrincipalId::new());
    let service_token = format!("service-{}", PrincipalId::new());
    let owner_token = format!("canonical-owner-{}", PrincipalId::new());

    seed_principal(
        &pool,
        tenant_id,
        tenant_admin_id,
        "TenantAdmin",
        &admin_token,
    )
    .await;
    seed_principal(&pool, tenant_id, service_id, "Service", &service_token).await;

    // The legacy Owner credential store remains independent. A canonical Owner
    // key_hash can authenticate through the Principal adapter without changing
    // the old Owner API token path.
    sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) VALUES ($1::uuid, $2::uuid, 'tenant_owner')",
    )
    .bind(tenant_id.to_string())
    .bind(owner_id.to_string())
    .execute(&pool)
    .await
    .expect("legacy Owner identity should be inserted");
    sqlx::query("UPDATE principals SET key_hash = $1 WHERE tenant_id = $2::uuid AND id = $3::uuid")
        .bind(Sha256::digest(owner_token.as_bytes()).to_vec())
        .bind(tenant_id.to_string())
        .bind(owner_id.to_string())
        .execute(&pool)
        .await
        .expect("canonical Owner credential hash should be updated");

    let authenticator = PrincipalAuthenticator::new(pool.clone());

    let admin = authenticate_principal(&authenticator, PrincipalCredentials::new(admin_token))
        .await
        .expect("TenantAdmin token should authenticate");
    assert_eq!(admin.tenant_id(), tenant_id);
    assert_eq!(admin.principal().principal_id(), tenant_admin_id);
    assert_eq!(admin.principal().kind(), PrincipalKind::Authenticated);

    let service = authenticate_principal(&authenticator, PrincipalCredentials::new(service_token))
        .await
        .expect("Service token should authenticate");
    assert_eq!(service.principal().principal_id(), service_id);
    assert_eq!(service.principal().kind(), PrincipalKind::Authenticated);

    let owner = authenticate_principal(&authenticator, PrincipalCredentials::new(owner_token))
        .await
        .expect("TenantOwner token should authenticate");
    assert_eq!(owner.principal().principal_id(), owner_id);
    assert_eq!(owner.principal().kind(), PrincipalKind::TenantOwner);

    let platform_id = PrincipalId::new();
    let platform_token = format!("platform-admin-{}", PrincipalId::new());
    sqlx::query(
        "INSERT INTO principals (id, tenant_id, kind, key_hash) VALUES ($1::uuid, NULL, 'PlatformAdmin', $2)",
    )
    .bind(platform_id.to_string())
    .bind(Sha256::digest(platform_token.as_bytes()).to_vec())
    .execute(&pool)
    .await
    .expect("PlatformAdmin fixture should be inserted");
    let platform =
        authenticate_principal(&authenticator, PrincipalCredentials::new(platform_token))
            .await
            .expect_err("PlatformAdmin must not establish tenant invocation Scope");
    assert!(matches!(
        platform,
        DomainAuthError::Rejected(StoreAuthError::InvalidCredentials)
    ));

    pool.close().await;
}
