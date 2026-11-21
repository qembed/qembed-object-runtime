mod common;

use qembed_domain::ids::{EventId, ObjectId, ObjectVersionId, SnapshotId, TenantId};
use qembed_store::run_migrations;

fn is_foreign_key_violation(error: sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|database_error| database_error.code())
        .as_deref()
        == Some("23503")
}

#[tokio::test]
async fn composite_foreign_keys_reject_cross_tenant_references() {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should be applied");

    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    let object_a = ObjectId::new();
    let object_b = ObjectId::new();
    let version_a = ObjectVersionId::new();

    sqlx::query("INSERT INTO objects (tenant_id, object_id) VALUES ($1::uuid, $2::uuid)")
        .bind(tenant_a.to_string())
        .bind(object_a.to_string())
        .execute(&pool)
        .await
        .expect("tenant A object should be inserted");

    sqlx::query(
        "INSERT INTO object_versions (tenant_id, object_id, object_version_id, version_number) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, 1)",
    )
    .bind(tenant_a.to_string())
    .bind(object_a.to_string())
    .bind(version_a.to_string())
    .execute(&pool)
    .await
    .expect("tenant A object version should be inserted");

    // A version row cannot borrow an object owned by another tenant, even
    // when the globally unique object ID is otherwise valid.
    let cross_tenant_version = sqlx::query(
        "INSERT INTO object_versions (tenant_id, object_id, object_version_id, version_number) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, 1)",
    )
    .bind(tenant_b.to_string())
    .bind(object_a.to_string())
    .bind(ObjectVersionId::new().to_string())
    .execute(&pool)
    .await
    .expect_err("composite parent FK must reject a cross-tenant object reference");
    assert!(is_foreign_key_violation(cross_tenant_version));

    // The published-version pointer is also tenant- and object-bound.
    let cross_tenant_pointer = sqlx::query(
        "INSERT INTO objects (tenant_id, object_id, published_version_id) \
         VALUES ($1::uuid, $2::uuid, $3::uuid)",
    )
    .bind(tenant_b.to_string())
    .bind(object_b.to_string())
    .bind(version_a.to_string())
    .execute(&pool)
    .await
    .expect_err("published-version FK must reject another tenant's version");
    assert!(is_foreign_key_violation(cross_tenant_pointer));

    // Snapshot provenance cannot point at a version from a different tenant.
    let cross_tenant_snapshot = sqlx::query(
        "INSERT INTO snapshots \
         (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, $5, $6, $7::jsonb)",
    )
    .bind(tenant_b.to_string())
    .bind(qembed_domain::ids::SnapshotId::new().to_string())
    .bind(object_a.to_string())
    .bind(version_a.to_string())
    .bind(br#"{}"#.as_slice())
    .bind(vec![0_u8; 32])
    .bind("{}")
    .execute(&pool)
    .await
    .expect_err("snapshot version FK must reject cross-tenant provenance");
    assert!(is_foreign_key_violation(cross_tenant_snapshot));

    // Outbox delivery identity is tenant-scoped and cannot reference an event
    // belonging to another tenant.
    let event_a = EventId::new();
    sqlx::query(
        "INSERT INTO events (tenant_id, event_id, aggregate_type, aggregate_id, event_type, payload) \
         VALUES ($1::uuid, $2::uuid, 'object', $3::uuid, 'object.created', '{}'::jsonb)",
    )
    .bind(tenant_a.to_string())
    .bind(event_a.to_string())
    .bind(object_a.to_string())
    .execute(&pool)
    .await
    .expect("tenant A event should be inserted");

    let cross_tenant_outbox = sqlx::query(
        "INSERT INTO outbox_messages (tenant_id, delivery_id, event_id, topic, payload) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, 'object.events', '{}'::jsonb)",
    )
    .bind(tenant_b.to_string())
    .bind(EventId::new().to_string())
    .bind(event_a.to_string())
    .execute(&pool)
    .await
    .expect_err("outbox event FK must reject a cross-tenant event reference");
    assert!(is_foreign_key_violation(cross_tenant_outbox));

    pool.close().await;
}


#[tokio::test]
async fn entity_identifiers_may_repeat_across_tenants_without_cross_linking() {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should be applied");

    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    let object_id = ObjectId::new();
    let version_id = ObjectVersionId::new();
    let snapshot_id = SnapshotId::new();
    let event_id = EventId::new();

    for tenant in [tenant_a, tenant_b] {
        sqlx::query("INSERT INTO objects (tenant_id, object_id) VALUES ($1::uuid, $2::uuid)")
            .bind(tenant.to_string())
            .bind(object_id.to_string())
            .execute(&pool)
            .await
            .expect("the same ObjectId should be valid in a different tenant");

        sqlx::query(
            "INSERT INTO object_versions (tenant_id, object_id, object_version_id, version_number) \
             VALUES ($1::uuid, $2::uuid, $3::uuid, 1)",
        )
        .bind(tenant.to_string())
        .bind(object_id.to_string())
        .bind(version_id.to_string())
        .execute(&pool)
        .await
        .expect("the same ObjectVersionId should be valid in a different tenant");

        sqlx::query(
            "INSERT INTO snapshots \
             (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) \
             VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, $5, $6, $7::jsonb)",
        )
        .bind(tenant.to_string())
        .bind(snapshot_id.to_string())
        .bind(object_id.to_string())
        .bind(version_id.to_string())
        .bind(br#"{}"#.as_slice())
        .bind(vec![0_u8; 32])
        .bind("{}")
        .execute(&pool)
        .await
        .expect("the same SnapshotId should be valid in a different tenant");

        sqlx::query(
            "INSERT INTO events (tenant_id, event_id, aggregate_type, aggregate_id, event_type, payload) \
             VALUES ($1::uuid, $2::uuid, 'object', $3::uuid, 'object.created', '{}'::jsonb)",
        )
        .bind(tenant.to_string())
        .bind(event_id.to_string())
        .bind(object_id.to_string())
        .execute(&pool)
        .await
        .expect("the same EventId should be valid in a different tenant");
    }

    let object_count: i64 = sqlx::query_scalar("SELECT count(*) FROM objects WHERE object_id = $1::uuid")
        .bind(object_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("object count should be readable");
    let version_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM object_versions WHERE object_version_id = $1::uuid")
            .bind(version_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("version count should be readable");
    let snapshot_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM snapshots WHERE snapshot_id = $1::uuid")
            .bind(snapshot_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("snapshot count should be readable");
    let event_count: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE event_id = $1::uuid")
        .bind(event_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("event count should be readable");

    assert_eq!(object_count, 2);
    assert_eq!(version_count, 2);
    assert_eq!(snapshot_count, 2);
    assert_eq!(event_count, 2);

    pool.close().await;
}
