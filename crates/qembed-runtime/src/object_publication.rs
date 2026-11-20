//! Trusted Owner use case for publishing a Draft object version.

use qembed_domain::{
    ids::{ObjectId, ObjectVersionId, SnapshotId},
    object::ObjectState,
    object_validation::{ObjectValidationError, ObjectValidator, ValidationReport},
    ports::ObjectPublicationStore,
    scope::Scope,
};

/// Successful publication plus the validator's explicit coverage report.
#[derive(Debug, PartialEq, Eq)]
pub struct ObjectPublicationResult {
    object_version_id: ObjectVersionId,
    snapshot_id: SnapshotId,
    report: ValidationReport,
}

impl ObjectPublicationResult {
    #[must_use]
    pub const fn object_version_id(&self) -> ObjectVersionId { self.object_version_id }

    #[must_use]
    pub const fn snapshot_id(&self) -> SnapshotId { self.snapshot_id }

    #[must_use]
    pub const fn report(&self) -> &ValidationReport { &self.report }
}

/// Publishes Owner-selected Draft versions only after trusted validation.
#[derive(Clone, Debug)]
pub struct ObjectPublicationService<S> {
    store: S,
}

impl<S: ObjectPublicationStore> ObjectPublicationService<S> {
    #[must_use]
    pub const fn new(store: S) -> Self { Self { store } }

    /// Validates the exact persisted snapshot and atomically publishes its version.
    ///
    /// First publication moves Draft -> Validating before validation and returns
    /// the object to Draft if validation rejects the candidate or publication
    /// fails. Replacements leave the currently active/suspended object untouched
    /// during validation; the Store checks the captured current pointer at commit.
    pub async fn publish(
        &self,
        scope: &Scope,
        object_id: ObjectId,
        version_id: ObjectVersionId,
    ) -> Result<ObjectPublicationResult, ObjectPublicationError<S::Error>> {
        let candidate = self.store
            .load_publication_candidate(scope, object_id, version_id)
            .await
            .map_err(ObjectPublicationError::Store)?
            .ok_or(ObjectPublicationError::NotFound)?;

        let first_publication = match (candidate.object_state(), candidate.expected_current()) {
            (ObjectState::Draft, None) => true,
            (ObjectState::Active | ObjectState::Suspended, Some(_)) => false,
            _ => return Err(ObjectPublicationError::InvalidObjectState),
        };

        if first_publication {
            self.store.begin_validation(scope, object_id)
                .await
                .map_err(ObjectPublicationError::Store)?;
        }

        let snapshot_id = candidate.snapshot().id();
        let outcome = match ObjectValidator::validate_with_report(candidate.snapshot()) {
            Ok(outcome) => outcome,
            Err(error) => {
                if first_publication {
                    self.store.reject_validation(scope, object_id)
                        .await
                        .map_err(ObjectPublicationError::Store)?;
                }
                return Err(ObjectPublicationError::ValidationRejected(error));
            }
        };
        let (evidence, report) = outcome.into_parts();

        if let Err(error) = self.store.publish_version(
            scope,
            object_id,
            version_id,
            candidate.expected_current(),
            &evidence,
        ).await {
            if first_publication {
                // Best-effort lifecycle recovery. The failed publication
                // transaction has already rolled back before this operation.
                self.store.reject_validation(scope, object_id)
                    .await
                    .map_err(ObjectPublicationError::Store)?;
            }
            return Err(ObjectPublicationError::Store(error));
        }

        Ok(ObjectPublicationResult {
            object_version_id: version_id,
            snapshot_id,
            report,
        })
    }
}

/// Failure while executing the trusted publication use case.
#[derive(Debug)]
pub enum ObjectPublicationError<E> {
    NotFound,
    InvalidObjectState,
    ValidationRejected(ObjectValidationError),
    Store(E),
}


#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        future::ready,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use qembed_domain::{
        ids::{ObjectVersionId, SnapshotId, TenantId},
        object::ObjectState,
        ports::{ObjectPublicationStore, PublicationCandidate},
        scope::Scope,
        snapshot::Snapshot,
        test_support::scope_for_tenant,
    };

    use super::*;

    #[derive(Default)]
    struct RecordingStore {
        candidate: Mutex<Option<PublicationCandidate>>,
        began: AtomicBool,
        rejected: AtomicBool,
        published: AtomicBool,
    }

    impl ObjectPublicationStore for RecordingStore {
        type Error = Infallible;

        fn load_publication_candidate(
            &self,
            _: &Scope,
            _: ObjectId,
            _: ObjectVersionId,
        ) -> impl Future<Output = Result<Option<PublicationCandidate>, Self::Error>> + Send {
            let candidate = self.candidate.lock().expect("candidate lock").take();
            ready(Ok(candidate))
        }

        fn begin_validation(
            &self,
            _: &Scope,
            _: ObjectId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.began.store(true, Ordering::SeqCst);
            ready(Ok(()))
        }

        fn reject_validation(
            &self,
            _: &Scope,
            _: ObjectId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.rejected.store(true, Ordering::SeqCst);
            ready(Ok(()))
        }

        fn publish_version(
            &self,
            _: &Scope,
            _: ObjectId,
            _: ObjectVersionId,
            _: Option<ObjectVersionId>,
            evidence: &qembed_domain::object_validation::ValidationEvidence,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            assert!(evidence.max_tokens_per_invocation() > 0);
            self.published.store(true, Ordering::SeqCst);
            ready(Ok(()))
        }
    }

    fn candidate(
        state: ObjectState,
        expected_current: Option<ObjectVersionId>,
        valid: bool,
    ) -> (PublicationCandidate, ObjectId, ObjectVersionId) {
        let tenant_id = TenantId::new();
        let object_id = ObjectId::new();
        let version_id = ObjectVersionId::new();
        let json = if valid {
            br#"{"limits":{"max_tokens_per_invocation":128}}"#.to_vec()
        } else {
            br#"{"limits":{}}"#.to_vec()
        };
        let snapshot = Snapshot::new(
            SnapshotId::new(),
            tenant_id,
            object_id,
            version_id,
            1,
            json,
        ).expect("snapshot bytes are canonical");
        (
            PublicationCandidate::new(snapshot, state, expected_current),
            object_id,
            version_id,
        )
    }

    #[tokio::test]
    async fn first_publication_enters_validation_then_publishes() {
        let (candidate, object_id, version_id) = candidate(ObjectState::Draft, None, true);
        let tenant_id = candidate.snapshot().tenant_id();
        let store = RecordingStore::default();
        *store.candidate.lock().expect("candidate lock") = Some(candidate);
        let service = ObjectPublicationService::new(store);
        let scope = scope_for_tenant(tenant_id);

        service
            .publish(&scope, object_id, version_id)
            .await
            .expect("valid first publication succeeds");

        assert!(service.store.began.load(Ordering::SeqCst));
        assert!(service.store.published.load(Ordering::SeqCst));
        assert!(!service.store.rejected.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn replacement_keeps_current_lifecycle_state_during_validation() {
        let current = ObjectVersionId::new();
        let (candidate, object_id, version_id) =
            candidate(ObjectState::Active, Some(current), true);
        let tenant_id = candidate.snapshot().tenant_id();
        let store = RecordingStore::default();
        *store.candidate.lock().expect("candidate lock") = Some(candidate);
        let service = ObjectPublicationService::new(store);
        let scope = scope_for_tenant(tenant_id);

        service
            .publish(&scope, object_id, version_id)
            .await
            .expect("valid replacement succeeds");

        assert!(!service.store.began.load(Ordering::SeqCst));
        assert!(service.store.published.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn rejected_first_publication_returns_object_to_draft() {
        let (candidate, object_id, version_id) = candidate(ObjectState::Draft, None, false);
        let tenant_id = candidate.snapshot().tenant_id();
        let store = RecordingStore::default();
        *store.candidate.lock().expect("candidate lock") = Some(candidate);
        let service = ObjectPublicationService::new(store);
        let scope = scope_for_tenant(tenant_id);

        assert!(matches!(
            service.publish(&scope, object_id, version_id).await,
            Err(ObjectPublicationError::ValidationRejected(
                ObjectValidationError::MissingMaxTokensPerInvocation
            ))
        ));
        assert!(service.store.began.load(Ordering::SeqCst));
        assert!(service.store.rejected.load(Ordering::SeqCst));
        assert!(!service.store.published.load(Ordering::SeqCst));
    }
}
