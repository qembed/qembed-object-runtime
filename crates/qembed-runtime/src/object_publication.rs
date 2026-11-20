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
