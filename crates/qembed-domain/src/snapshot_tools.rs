//! Typed references to immutable ToolVersions captured by a Snapshot.

use std::collections::HashSet;

use crate::ids::{ToolId, ToolVersionId};
use crate::snapshot::Snapshot;

/// The exact Tool and ToolVersion pair captured in one immutable Snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SnapshotToolReference {
    tool_id: ToolId,
    tool_version_id: ToolVersionId,
}

impl SnapshotToolReference {
    #[must_use]
    pub const fn new(tool_id: ToolId, tool_version_id: ToolVersionId) -> Self {
        Self {
            tool_id,
            tool_version_id,
        }
    }

    #[must_use]
    pub const fn tool_id(self) -> ToolId {
        self.tool_id
    }

    #[must_use]
    pub const fn tool_version_id(self) -> ToolVersionId {
        self.tool_version_id
    }
}

/// Parses the Snapshot's canonical `tools` array without consulting mutable bindings.
///
/// A missing `tools` field means that the Snapshot contains no tool bindings.
/// If present, each entry must contain valid UUID strings for both identities.
pub fn snapshot_tool_references(
    snapshot: &Snapshot,
) -> Result<Vec<SnapshotToolReference>, SnapshotToolReferencesError> {
    let value: serde_json::Value = serde_json::from_slice(snapshot.canonical_json())
        .map_err(|_| SnapshotToolReferencesError::InvalidSnapshotJson)?;
    let Some(tools) = value.get("tools") else {
        return Ok(Vec::new());
    };
    let entries = tools
        .as_array()
        .ok_or(SnapshotToolReferencesError::ToolsMustBeArray)?;

    let mut seen = HashSet::with_capacity(entries.len());
    let mut references = Vec::with_capacity(entries.len());
    for entry in entries {
        let object = entry
            .as_object()
            .ok_or(SnapshotToolReferencesError::InvalidToolEntry)?;
        let tool_id = object
            .get("tool_id")
            .and_then(serde_json::Value::as_str)
            .ok_or(SnapshotToolReferencesError::InvalidToolId)?
            .parse()
            .map_err(|_| SnapshotToolReferencesError::InvalidToolId)?;
        let tool_version_id = object
            .get("tool_version_id")
            .and_then(serde_json::Value::as_str)
            .ok_or(SnapshotToolReferencesError::InvalidToolVersionId)?
            .parse()
            .map_err(|_| SnapshotToolReferencesError::InvalidToolVersionId)?;

        if !seen.insert(tool_id) {
            return Err(SnapshotToolReferencesError::DuplicateToolId);
        }
        references.push(SnapshotToolReference::new(tool_id, tool_version_id));
    }
    Ok(references)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotToolReferencesError {
    InvalidSnapshotJson,
    ToolsMustBeArray,
    InvalidToolEntry,
    InvalidToolId,
    InvalidToolVersionId,
    DuplicateToolId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ObjectId, ObjectVersionId, SnapshotId, TenantId};

    fn snapshot(json: &[u8]) -> Snapshot {
        Snapshot::new(
            SnapshotId::new(),
            TenantId::new(),
            ObjectId::new(),
            ObjectVersionId::new(),
            1,
            json.to_vec(),
        )
        .expect("test JSON is canonical")
    }

    #[test]
    fn missing_tools_means_no_tool_references() {
        assert!(
            snapshot_tool_references(&snapshot(br#"{"limits":{}}"#))
                .expect("valid snapshot")
                .is_empty()
        );
    }

    #[test]
    fn reads_exact_tool_version_ids_from_snapshot() {
        let tool = ToolId::new();
        let version = ToolVersionId::new();
        let json = format!(r#"{{"tools":[{{"tool_id":"{tool}","tool_version_id":"{version}"}}]}}"#);
        let refs = snapshot_tool_references(&snapshot(json.as_bytes())).expect("valid references");
        assert_eq!(refs, vec![SnapshotToolReference::new(tool, version)]);
    }

    #[test]
    fn rejects_duplicate_tool_ids_and_malformed_versions() {
        let tool = ToolId::new();
        let version = ToolVersionId::new();
        let duplicate = format!(
            r#"{{"tools":[{{"tool_id":"{tool}","tool_version_id":"{version}"}},{{"tool_id":"{tool}","tool_version_id":"{version}"}}]}}"#
        );
        assert_eq!(
            snapshot_tool_references(&snapshot(duplicate.as_bytes())),
            Err(SnapshotToolReferencesError::DuplicateToolId)
        );
        assert_eq!(
            snapshot_tool_references(&snapshot(
                br#"{"tools":[{"tool_id":"bad","tool_version_id":"bad"}]}"#
            )),
            Err(SnapshotToolReferencesError::InvalidToolId)
        );
    }
}
