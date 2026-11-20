use actix_web::{App, http::StatusCode, test};
use qembed_domain::ids::{ObjectId, ObjectVersionId, PrincipalId, TenantId};
use qembed_store::{PgPoolConfig, connect_pool, run_migrations};
use sha2::{Digest, Sha256};

use qembed_api::composition::configure_owner_routes;

async fn insert_owner(pool: &sqlx::PgPool, tenant_id: TenantId, token: &str) {
    let principal_id = PrincipalId::new();
    sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) VALUES ($1::uuid, $2::uuid, 'tenant_owner')",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .execute(pool)
    .await
    .expect("owner principal fixture should be inserted");
    sqlx::query(
        "INSERT INTO owner_credentials (tenant_id, caller_id, token_hash) VALUES ($1::uuid, $2::uuid, $3)",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .bind(Sha256::digest(token.as_bytes()).to_vec())
    .execute(pool)
    .await
    .expect("owner credential fixture should be inserted");
}

async fn create_object(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    tenant_id: TenantId,
    token: &str,
) -> ObjectId {
    let response = test::call_service(
        app,
        test::TestRequest::post()
            .uri("/owner/objects")
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request(),
    ).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let json: serde_json::Value = serde_json::from_slice(&test::read_body(response).await)
        .expect("object response is JSON");
    json["object_id"].as_str().expect("object ID").parse().expect("valid object ID")
}

async fn create_version(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    tenant_id: TenantId,
    token: &str,
    object_id: ObjectId,
    config: &str,
) -> ObjectVersionId {
    let response = test::call_service(
        app,
        test::TestRequest::post()
            .uri(&format!("/owner/objects/{object_id}/versions"))
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .insert_header((actix_web::http::header::CONTENT_TYPE, "application/json"))
            .set_payload(format!(r#"{{"schema_version":1,"config":{config}}}"#))
            .to_request(),
    ).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let json: serde_json::Value = serde_json::from_slice(&test::read_body(response).await)
        .expect("version response is JSON");
    json["object_version_id"].as_str().expect("version ID").parse().expect("valid version ID")
}

async fn publish(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    tenant_id: TenantId,
    token: &str,
    object_id: ObjectId,
    version_id: ObjectVersionId,
) -> actix_web::dev::ServiceResponse {
    test::call_service(
        app,
        test::TestRequest::post()
            .uri(&format!("/owner/objects/{object_id}/versions/{version_id}/publish"))
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request(),
    ).await
}

#[actix_web::test]
async fn owner_publication_validates_and_publishes_first_version_and_replacement() {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by the PostgreSQL integration-test job");
    let pool = connect_pool(&database_url, PgPoolConfig::default())
        .await.expect("PostgreSQL should accept connections");
    run_migrations(&pool).await.expect("migrations should be applied");

    let tenant_id = TenantId::new();
    let other_tenant = TenantId::new();
    let token = format!("owner-publish-{}", PrincipalId::new());
    let other_token = format!("owner-publish-{}", PrincipalId::new());
    insert_owner(&pool, tenant_id, &token).await;
    insert_owner(&pool, other_tenant, &other_token).await;

    let app = test::init_service(
        App::new().configure(|config| configure_owner_routes(config, pool.clone())),
    ).await;

    let object_id = create_object(&app, tenant_id, &token).await;
    let first = create_version(
        &app, tenant_id, &token, object_id,
        r#"{"limits":{"max_tokens_per_invocation":4096},"revision":"first"}"#,
    ).await;

    let first_response = publish(&app, tenant_id, &token, object_id, first).await;
    assert_eq!(first_response.status(), StatusCode::OK);
    let first_json: serde_json::Value = serde_json::from_slice(&test::read_body(first_response).await)
        .expect("publication response is JSON");
    assert_eq!(first_json["status"], "published");
    assert_eq!(first_json["object_version_id"], first.to_string());
    assert_eq!(first_json["validator_version"], "m0-r3-bounded-budget-v1");
    assert_eq!(first_json["validation"]["coverage"][0]["status"], "validated");
    assert_eq!(first_json["validation"]["coverage"][1]["status"], "not_validated_specification_gap");

    let first_state: (String, Option<String>) = sqlx::query_as(
        "SELECT state, published_version_id::text FROM objects WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    ).bind(tenant_id.to_string()).bind(object_id.to_string()).fetch_one(&pool).await
        .expect("first publication should persist");
    assert_eq!(first_state, ("active".to_owned(), Some(first.to_string())));

    let second = create_version(
        &app, tenant_id, &token, object_id,
        r#"{"limits":{"max_tokens_per_invocation":2048},"revision":"second"}"#,
    ).await;
    let second_response = publish(&app, tenant_id, &token, object_id, second).await;
    assert_eq!(second_response.status(), StatusCode::OK);
    let second_json: serde_json::Value = serde_json::from_slice(&test::read_body(second_response).await)
        .expect("replacement response is JSON");
    assert_eq!(second_json["object_version_id"], second.to_string());

    let final_state: (String, Option<String>) = sqlx::query_as(
        "SELECT state, published_version_id::text FROM objects WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    ).bind(tenant_id.to_string()).bind(object_id.to_string()).fetch_one(&pool).await
        .expect("replacement should persist");
    assert_eq!(final_state, ("active".to_owned(), Some(second.to_string())));

    let statuses: Vec<(String,)> = sqlx::query_as(
        "SELECT status FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid ORDER BY version_number",
    ).bind(tenant_id.to_string()).bind(object_id.to_string()).fetch_all(&pool).await
        .expect("version statuses should be queryable");
    assert_eq!(statuses, vec![("superseded".to_owned(),), ("published".to_owned(),)]);

    let invalid = create_version(&app, tenant_id, &token, object_id, r#"{"limits":{}}"#).await;
    let rejected = publish(&app, tenant_id, &token, object_id, invalid).await;
    assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let invalid_state: (String, String, Option<String>) = sqlx::query_as(
        "SELECT o.state, v.status, o.published_version_id::text FROM objects o JOIN object_versions v USING (tenant_id, object_id) WHERE o.tenant_id = $1::uuid AND o.object_id = $2::uuid AND v.object_version_id = $3::uuid",
    ).bind(tenant_id.to_string()).bind(object_id.to_string()).bind(invalid.to_string()).fetch_one(&pool).await
        .expect("rejected version state should be queryable");
    assert_eq!(invalid_state, ("active".to_owned(), "draft".to_owned(), Some(second.to_string())));

    let cross_tenant = publish(&app, other_tenant, &other_token, object_id, first).await;
    assert_eq!(cross_tenant.status(), StatusCode::NOT_FOUND);

    drop(app);
    pool.close().await;
}
