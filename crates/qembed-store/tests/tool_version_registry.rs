mod common;

use qembed_domain::{
    ids::{ToolId, ToolVersionId},
    ports::ToolVersionRegistry,
    tool_version::{ToolDefinition, ToolRiskLevel, ToolVersion},
};
use qembed_store::{run_migrations, tool_version_registry::PgToolVersionRegistry};

#[tokio::test]
async fn registry_loads_exact_immutable_version_and_rejects_mutation() {
    let pool = common::connect().await;
    run_migrations(&pool).await.expect("migrations should run");
    let registry = PgToolVersionRegistry::new(pool.clone());

    let tool_id = ToolId::new();
    let definition =
        ToolDefinition::new(tool_id, "registry-test", "test tool").expect("definition is valid");
    registry
        .register_definition(&definition)
        .await
        .expect("definition should be registered");

    let version_id = ToolVersionId::new();
    let version = ToolVersion::new(
        version_id,
        tool_id,
        "1.0.0",
        ToolRiskLevel::R0,
        serde_json::json!({"type":"object"}),
        serde_json::json!({"type":"object"}),
        2_000,
        false,
    )
    .expect("version is valid");
    registry
        .register_version(&version)
        .await
        .expect("immutable version should be registered");

    let loaded = registry
        .get_tool_version(tool_id, version_id)
        .await
        .expect("registry read should succeed")
        .expect("registered version should exist");
    assert_eq!(loaded, version);

    let wrong_tool = ToolId::new();
    assert!(
        registry
            .get_tool_version(wrong_tool, version_id)
            .await
            .expect("wrong-tool lookup should succeed")
            .is_none()
    );

    let mutation = sqlx::query("UPDATE tool_versions SET version='2.0.0' WHERE id=$1::uuid")
        .bind(version_id.to_string())
        .execute(&pool)
        .await;
    assert!(
        mutation.is_err(),
        "immutable ToolVersion must reject UPDATE"
    );

    let unchanged = registry
        .get_tool_version(tool_id, version_id)
        .await
        .expect("registry read should succeed")
        .expect("original version must remain");
    assert_eq!(unchanged.version(), "1.0.0");
    pool.close().await;
}
