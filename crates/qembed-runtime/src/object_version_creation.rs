//! Owner use case for creating an immutable Draft version and snapshot.

use qembed_domain::{
    ids::{ObjectId, ObjectVersionId, SnapshotId},
    object_version::{ObjectVersion, ObjectVersionError},
    ports::{DraftVersionCreation, ObjectVersionCreationStore},
    scope::Scope,
    snapshot::{Snapshot, SnapshotError},
};

/// Creates tenant-owned Draft versions through the atomic persistence port.
#[derive(Clone, Debug)]
pub struct ObjectVersionCreationService<S> {
    store: S,
}

impl<S: ObjectVersionCreationStore> ObjectVersionCreationService<S> {
    /// Creates the use case with its persistence adapter.
    #[must_use]
    pub const fn new(store: S) -> Self {
        Self { store }
    }

    /// Persists a canonical configuration snapshot as a Draft version.
    ///
    /// Version numbering is allocated by the Store while holding the object row
    /// lock. The placeholder number is never persisted by this service.
    pub async fn create(
        &self,
        scope: &Scope,
        object_id: ObjectId,
        schema_version: u32,
        canonical_json: Vec<u8>,
    ) -> Result<DraftVersionCreation, ObjectVersionCreationError<S::Error>> {
        let version_id = ObjectVersionId::new();
        let version = ObjectVersion::new(version_id, scope.tenant_id(), object_id, 1)
            .map_err(ObjectVersionCreationError::InvalidVersion)?;
        let snapshot = Snapshot::new(
            SnapshotId::new(),
            scope.tenant_id(),
            object_id,
            version_id,
            schema_version,
            canonical_json,
        )
        .map_err(ObjectVersionCreationError::InvalidSnapshot)?;

        self.store
            .create_draft_version_with_snapshot(scope, &version, &snapshot)
            .await
            .map_err(ObjectVersionCreationError::Store)
    }
}

/// Failure while executing the Draft-version creation use case.
#[derive(Debug)]
pub enum ObjectVersionCreationError<E> {
    InvalidVersion(ObjectVersionError),
    InvalidSnapshot(SnapshotError),
    Store(E),
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::future::{Future, ready};
    use qembed_domain::{
        ids::TenantId,
        ports::{DraftVersionCreation, ObjectVersionCreationStore},
        scope::Scope,
        snapshot::Snapshot,
        test_support::scope_for_tenant,
    };

    use super::*;

    struct RecordingStore;

    impl ObjectVersionCreationStore for RecordingStore {
        type Error = Infallible;

        fn create_draft_version_with_snapshot(
            &self,
            _: &Scope,
            version: &ObjectVersion,
            snapshot: &Snapshot,
        ) -> impl Future<Output = Result<DraftVersionCreation, Self::Error>> + Send {
            let version_id = version.id();
            let snapshot_id = snapshot.id();
            let result = DraftVersionCreation::Created {
                version_id,
                snapshot_id,
            };
            ready(Ok(result))
        }
    }

    #[tokio::test]
    async fn create_builds_draft_version_and_canonical_snapshot_for_scope_tenant() {
        let tenant_id = TenantId::new();
        let scope = scope_for_tenant(tenant_id);
        let service = ObjectVersionCreationService::new(RecordingStore);
        let object_id = ObjectId::new();

        let result = service
            .create(
                &scope,
                object_id,
                1,
                br#"{"config":{"enabled":true}}"#.to_vec(),
            )
            .await
            .expect("canonical draft creation succeeds");

        let (version_id, snapshot_id) = match result {
            DraftVersionCreation::Created { version_id, snapshot_id } => (version_id, snapshot_id),
            other => panic!("recording store should report creation, got {other:?}"),
        };
        assert_eq!(version_id.to_string().len(), 36);
        assert_eq!(snapshot_id.to_string().len(), 36);
    }

    #[tokio::test]
    async fn create_rejects_noncanonical_snapshot_bytes() {
        let scope = scope_for_tenant(TenantId::new());
        let service = ObjectVersionCreationService::new(RecordingStore);

        assert!(matches!(
            service.create(&scope, ObjectId::new(), 1, br#"{ "x":1}"#.to_vec()).await,
            Err(ObjectVersionCreationError::InvalidSnapshot(SnapshotError::Jcs(_)))
        ));
    }
}
