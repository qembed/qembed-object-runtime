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
    .bind(Sha256::digest(token.as_bytes()).to_vec())
    .execute(pool)
    .await
    .expect("owner credential fixture should be inserted");
}

#[actix_web::test]
async fn owner_publication_validates_first_version_and_replacement() {
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

    let object_response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/owner/objects")
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request(),
    ).await;
    assert_eq!(object_response.status(), StatusCode::CREATED);
    let object_json: serde_json::Value = serde_json::from_slice(&test::read_body(object_response).await)
        .expect("object response is JSON");
    let object_id = object_json["object_id"].as_str().expect("object ID")
        .parse::<ObjectId>().expect("valid object ID");

    let first_create = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/owner/objects/{object_id}/versions"))
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .insert_header((actix_web::http::header::CONTENT_TYPE, "application/json"))
            .set_payload(r#"{"schema_version":1,"config":{"limits":{"max_tokens_per_invocation":4096},"revision":"first"}}"#)
            .to_request(),
    ).await;
    assert_eq!(first_create.status(), StatusCode::CREATED);
    let first_json: serde_json::Value = serde_json::from_slice(&test::read_body(first_create).await)
        .expect("version response is JSON");
    let first = first_json["object_version_id"].as_str().expect("version ID")
        .parse::<ObjectVersionId>().expect("valid version ID");

    let first_publish = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/owner/objects/{object_id}/versions/{first}/publish"))
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request(),
    ).await;
    assert_eq!(first_publish.status(), StatusCode::OK);
    let first_result: serde_json::Value = serde_json::from_slice(&test::read_body(first_publish).await)
        .expect("publication response is JSON");
    assert_eq!(first_result["status"], "published");
    assert_eq!(first_result["object_version_id"], first.to_string());
    assert_eq!(first_result["validator_version"], "m0-r3-bounded-budget-v1");
    assert_eq!(first_result["validation"]["coverage"][0]["status"], "validated");
    assert_eq!(first_result["validation"]["coverage"][1]["status"], "not_validated_specification_gap");

    let first_state: (String, Option<String>) = sqlx::query_as(
        "SELECT state, published_version_id::text FROM objects WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    ).bind(tenant_id.to_string()).bind(object_id.to_string()).fetch_one(&pool).await
        .expect("first publication should persist");
    assert_eq!(first_state, ("active".to_owned(), Some(first.to_string())));

    let second_create = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/owner/objects/{object_id}/versions"))
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .insert_header((actix_web::http::header::CONTENT_TYPE, "application/json"))
            .set_payload(r#"{"schema_version":1,"config":{"limits":{"max_tokens_per_invocation":2048},"revision":"second"}}"#)
            .to_request(),
    ).await;
    assert_eq!(second_create.status(), StatusCode::CREATED);
    let second_json: serde_json::Value = serde_json::from_slice(&test::read_body(second_create).await)
        .expect("version response is JSON");
    let second = second_json["object_version_id"].as_str().expect("version ID")
        .parse::<ObjectVersionId>().expect("valid version ID");

    let second_publish = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/owner/objects/{object_id}/versions/{second}/publish"))
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request(),
    ).await;
    assert_eq!(second_publish.status(), StatusCode::OK);
    let _: serde_json::Value = serde_json::from_slice(&test::read_body(second_publish).await)
        .expect("replacement response is JSON");

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

    let invalid_create = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/owner/objects/{object_id}/versions"))
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .insert_header((actix_web::http::header::CONTENT_TYPE, "application/json"))
            .set_payload(r#"{"schema_version":1,"config":{"limits":{}}}"#)
            .to_request(),
    ).await;
    assert_eq!(invalid_create.status(), StatusCode::CREATED);
    let invalid_json: serde_json::Value = serde_json::from_slice(&test::read_body(invalid_create).await)
        .expect("draft response is JSON");
    let invalid = invalid_json["object_version_id"].as_str().expect("version ID")
        .parse::<ObjectVersionId>().expect("valid version ID");

    let rejected = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/owner/objects/{object_id}/versions/{invalid}/publish"))
            .insert_header(("X-Tenant-ID", tenant_id.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request(),
    ).await;
    assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let invalid_state: (String, String, Option<String>) = sqlx::query_as(
        "SELECT o.state, v.status, o.published_version_id::text FROM objects o JOIN object_versions v USING (tenant_id, object_id) WHERE o.tenant_id = $1::uuid AND o.object_id = $2::uuid AND v.object_version_id = $3::uuid",
    ).bind(tenant_id.to_string()).bind(object_id.to_string()).bind(invalid.to_string()).fetch_one(&pool).await
        .expect("rejected version state should be queryable");
    assert_eq!(invalid_state, ("active".to_owned(), "draft".to_owned(), Some(second.to_string())));

    let cross_tenant = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/owner/objects/{object_id}/versions/{first}/publish"))
            .insert_header(("X-Tenant-ID", other_tenant.to_string()))
            .insert_header((actix_web::http::header::AUTHORIZATION, format!("Bearer {other_token}")))
            .to_request(),
    ).await;
    assert_eq!(cross_tenant.status(), StatusCode::NOT_FOUND);

    drop(app);
    pool.close().await;
}
