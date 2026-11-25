//! Resolves the exact immutable ToolVersions captured by a Snapshot.

use std::{error::Error, fmt};

use qembed_domain::{
    ids::{ToolId, ToolVersionId},
    ports::ToolVersionRegistry,
    snapshot::Snapshot,
    snapshot_tools::{SnapshotToolReferencesError, snapshot_tool_references},
    tool_version::ToolVersion,
};

/// Resolves every tool reference in a Snapshot against the immutable registry.
///
/// This deliberately does not read `object_tools`: that table describes the
/// current binding and must not rewrite the version captured by an old Snapshot.
pub async fn resolve_snapshot_tools<R: ToolVersionRegistry>(
    registry: &R,
    snapshot: &Snapshot,
) -> Result<Vec<ToolVersion>, ToolVersionResolutionError<R::Error>> {
    let references = snapshot_tool_references(snapshot)
        .map_err(ToolVersionResolutionError::InvalidSnapshotReferences)?;
    let mut resolved = Vec::with_capacity(references.len());

    for reference in references {
        let tool_id = reference.tool_id();
        let version_id = reference.tool_version_id();
        let version = registry
            .get_tool_version(tool_id, version_id)
            .await
            .map_err(ToolVersionResolutionError::Registry)?
            .ok_or(ToolVersionResolutionError::VersionNotFound {
                tool_id,
                version_id,
            })?;

        if version.tool_id() != tool_id || version.id() != version_id {
            return Err(ToolVersionResolutionError::RegistryIdentityMismatch {
                expected_tool_id: tool_id,
                expected_version_id: version_id,
            });
        }
        resolved.push(version);
    }

    Ok(resolved)
}

#[derive(Debug)]
pub enum ToolVersionResolutionError<E> {
    InvalidSnapshotReferences(SnapshotToolReferencesError),
    Registry(E),
    VersionNotFound {
        tool_id: ToolId,
        version_id: ToolVersionId,
    },
    RegistryIdentityMismatch {
        expected_tool_id: ToolId,
        expected_version_id: ToolVersionId,
    },
}

impl<E: fmt::Display> fmt::Display for ToolVersionResolutionError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSnapshotReferences(error) => {
                write!(formatter, "invalid Snapshot tool references: {error:?}")
            }
            Self::Registry(error) => write!(formatter, "ToolVersion registry failed: {error}"),
            Self::VersionNotFound {
                tool_id,
                version_id,
            } => write!(
                formatter,
                "ToolVersion {version_id} for Tool {tool_id} was not found"
            ),
            Self::RegistryIdentityMismatch {
                expected_tool_id,
                expected_version_id,
            } => write!(
                formatter,
                "registry returned a different identity than {expected_tool_id}/{expected_version_id}"
            ),
        }
    }
}

impl<E: Error + 'static> Error for ToolVersionResolutionError<E> {}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, convert::Infallible, future::Future};

    use qembed_domain::{
        ids::{ObjectId, ObjectVersionId, SnapshotId, TenantId, ToolId, ToolVersionId},
        snapshot::Snapshot,
        tool_version::{ToolRiskLevel, ToolVersion},
    };

    use super::*;

    #[derive(Default)]
    struct MockRegistry(HashMap<(ToolId, ToolVersionId), ToolVersion>);

    impl ToolVersionRegistry for MockRegistry {
        type Error = Infallible;

        fn get_tool_version(
            &self,
            tool_id: ToolId,
            version_id: ToolVersionId,
        ) -> impl Future<Output = Result<Option<ToolVersion>, Self::Error>> + Send {
            async move { Ok(self.0.get(&(tool_id, version_id)).cloned()) }
        }
    }

    fn tool_version(tool_id: ToolId, version_id: ToolVersionId, label: &str) -> ToolVersion {
        ToolVersion::new(
            version_id,
            tool_id,
            label,
            ToolRiskLevel::R0,
            serde_json::json!({"type":"object"}),
            serde_json::json!({"type":"object"}),
            2_000,
            true,
        )
        .expect("valid ToolVersion")
    }

    fn snapshot(tool_id: ToolId, version_id: ToolVersionId) -> Snapshot {
        let json =
            format!(r#"{{"tools":[{{"tool_id":"{tool_id}","tool_version_id":"{version_id}"}}]}}"#);
        Snapshot::new(
            SnapshotId::new(),
            TenantId::new(),
            ObjectId::new(),
            ObjectVersionId::new(),
            1,
            json.into_bytes(),
        )
        .expect("canonical Snapshot")
    }

    #[tokio::test]
    async fn resolves_the_version_pinned_in_snapshot_not_a_newer_binding() {
        let tool = ToolId::new();
        let old_id = ToolVersionId::new();
        let new_id = ToolVersionId::new();
        let old = tool_version(tool, old_id, "1");
        let newer = tool_version(tool, new_id, "2");
        let mut registry = MockRegistry::default();
        registry.0.insert((tool, old_id), old.clone());
        registry.0.insert((tool, new_id), newer);

        let resolved = resolve_snapshot_tools(&registry, &snapshot(tool, old_id))
            .await
            .expect("pinned version exists");
        assert_eq!(resolved, vec![old]);
    }

    #[tokio::test]
    async fn missing_or_wrong_tool_version_fails_closed() {
        let tool = ToolId::new();
        let missing = ToolVersionId::new();
        let error = resolve_snapshot_tools(&MockRegistry::default(), &snapshot(tool, missing))
            .await
            .expect_err("missing immutable version must not fall back");
        assert!(matches!(
            error,
            ToolVersionResolutionError::VersionNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn empty_snapshot_does_not_query_registry() {
        let snapshot = Snapshot::new(
            SnapshotId::new(),
            TenantId::new(),
            ObjectId::new(),
            ObjectVersionId::new(),
            1,
            br#"{"limits":{}}"#.to_vec(),
        )
        .expect("canonical Snapshot");
        let resolved = resolve_snapshot_tools(&MockRegistry::default(), &snapshot)
            .await
            .expect("empty tools list is valid");
        assert!(resolved.is_empty());
    }
}
