mod common;

use qembed_domain::auth::{OwnerAuthenticationError as DomainAuthError, authenticate_owner};
use qembed_domain::ids::{CallerId, TenantId};
use qembed_domain::principal::PrincipalKind;
use qembed_store::auth::{OwnerAuthenticationError, OwnerAuthenticator, OwnerCredentials};
use qembed_store::run_migrations;
use sha2::{Digest, Sha256};

async fn seed_owner(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    caller_id: CallerId,
    token: &str,
) {
    sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) VALUES ($1::uuid, $2::uuid, 'tenant_owner')",
    )
    .bind(tenant_id.to_string())
    .bind(caller_id.to_string())
    .execute(pool)
    .await
    .expect("owner principal fixture should be inserted");

    let token_hash = Sha256::digest(token.as_bytes()).to_vec();
    sqlx::query(
        "INSERT INTO owner_credentials (tenant_id, caller_id, token_hash) VALUES ($1::uuid, $2::uuid, $3)",
    )
    .bind(tenant_id.to_string())
    .bind(caller_id.to_string())
    .bind(token_hash)
    .execute(pool)
    .await
    .expect("owner credential fixture should be inserted");
}

#[tokio::test]
async fn owner_authentication_enforces_owner_and_tenant_boundaries() {
    let pool = common::connect().await;
    run_migrations(&pool).await.expect("migrations should be applied");

    // Valid credentials resolve the persisted caller and tenant into a trusted Scope.
    let tenant_a = TenantId::new();
    let caller_a = CallerId::new();
    let token_a = format!("owner-token-{}", CallerId::new());
    seed_owner(&pool, tenant_a, caller_a, &token_a).await;

    let authenticator = OwnerAuthenticator::new(pool.clone());
    let scope = authenticate_owner(
        &authenticator,
        OwnerCredentials::new(tenant_a, token_a.clone()),
    )
    .await
    .expect("valid owner credentials should authenticate");

    assert_eq!(scope.tenant_id(), tenant_a);
    assert_eq!(scope.principal().caller_id(), caller_a);
    assert_eq!(scope.principal().kind(), PrincipalKind::TenantOwner);

    // Raw API credentials are never persisted; only the fixed-size digest is stored.
    let stored_hash: Vec<u8> = sqlx::query_scalar(
        "SELECT token_hash FROM owner_credentials WHERE tenant_id = $1::uuid AND caller_id = $2::uuid",
    )
    .bind(tenant_a.to_string())
    .bind(caller_a.to_string())
    .fetch_one(&pool)
    .await
    .expect("credential digest should exist");
    assert_eq!(stored_hash, Sha256::digest(token_a.as_bytes()).to_vec());
    assert_eq!(stored_hash.len(), 32);

    // Invalid credentials produce the same public rejection as disabled or mismatched owners.
    let invalid = authenticate_owner(
        &authenticator,
        OwnerCredentials::new(tenant_a, "incorrect-token"),
    )
    .await
    .expect_err("unknown token should be rejected");
    assert!(matches!(
        invalid,
        DomainAuthError::Rejected(OwnerAuthenticationError::InvalidCredentials)
    ));

    // A disabled principal cannot authenticate even with its correct token.
    let disabled_tenant = TenantId::new();
    let disabled_caller = CallerId::new();
    let disabled_token = format!("disabled-token-{}", CallerId::new());
    seed_owner(&pool, disabled_tenant, disabled_caller, &disabled_token).await;
    sqlx::query(
        "UPDATE owner_principals SET disabled_at = now() WHERE tenant_id = $1::uuid AND caller_id = $2::uuid",
    )
    .bind(disabled_tenant.to_string())
    .bind(disabled_caller.to_string())
    .execute(&pool)
    .await
    .expect("principal should be disabled");

    let disabled = authenticate_owner(
        &authenticator,
        OwnerCredentials::new(disabled_tenant, disabled_token),
    )
    .await
    .expect_err("disabled principal must not authenticate");
    assert!(matches!(
        disabled,
        DomainAuthError::Rejected(OwnerAuthenticationError::InvalidCredentials)
    ));

    // A valid token cannot be replayed under another tenant ID.
    let tenant_b = TenantId::new();
    let mismatch = authenticate_owner(
        &authenticator,
        OwnerCredentials::new(tenant_b, token_a),
    )
    .await
    .expect_err("tenant A token must not authenticate as tenant B");
    assert!(matches!(
        mismatch,
        DomainAuthError::Rejected(OwnerAuthenticationError::InvalidCredentials)
    ));

    pool.close().await;
}
