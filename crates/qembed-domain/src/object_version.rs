//! Object version entity and publication lifecycle.

use crate::ids::{ObjectId, ObjectVersionId, TenantId};

/// Publication lifecycle of an object version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectVersionStatus {
    Draft,
    Published,
    Superseded,
}

impl ObjectVersionStatus {
    /// Returns whether the lifecycle permits a transition to `next`.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Draft, Self::Published) | (Self::Published, Self::Superseded)
        )
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Superseded)
    }
}

/// A numbered, tenant-owned version of an object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectVersion {
    id: ObjectVersionId,
    tenant_id: TenantId,
    object_id: ObjectId,
    version_number: u64,
    status: ObjectVersionStatus,
}

impl ObjectVersion {
    pub fn new(
        id: ObjectVersionId,
        tenant_id: TenantId,
        object_id: ObjectId,
        version_number: u64,
    ) -> Result<Self, ObjectVersionError> {
        if version_number == 0 {
            return Err(ObjectVersionError::InvalidVersionNumber);
        }

        Ok(Self {
            id,
            tenant_id,
            object_id,
            version_number,
            status: ObjectVersionStatus::Draft,
        })
    }

    /// Reconstitutes a version from persisted state.
    pub fn from_persisted(
        id: ObjectVersionId,
        tenant_id: TenantId,
        object_id: ObjectId,
        version_number: u64,
        status: ObjectVersionStatus,
    ) -> Result<Self, ObjectVersionError> {
        let mut version = Self::new(id, tenant_id, object_id, version_number)?;
        version.status = status;
        Ok(version)
    }

    #[must_use]
    pub const fn id(&self) -> ObjectVersionId {
        self.id
    }

    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    #[must_use]
    pub const fn object_id(&self) -> ObjectId {
        self.object_id
    }

    #[must_use]
    pub const fn version_number(&self) -> u64 {
        self.version_number
    }

    #[must_use]
    pub const fn status(&self) -> ObjectVersionStatus {
        self.status
    }

    /// Publishes a draft version.
    pub fn publish(&mut self) -> Result<(), ObjectVersionError> {
        self.transition_to(ObjectVersionStatus::Published)
    }

    /// Supersedes a published version without modifying its content.
    pub fn supersede(&mut self) -> Result<(), ObjectVersionError> {
        self.transition_to(ObjectVersionStatus::Superseded)
    }

    fn transition_to(&mut self, next: ObjectVersionStatus) -> Result<(), ObjectVersionError> {
        if !self.status.can_transition_to(next) {
            return Err(ObjectVersionError::InvalidTransition {
                from: self.status,
                to: next,
            });
        }

        self.status = next;
        Ok(())
    }
}

/// Object-version invariant or transition error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectVersionError {
    InvalidVersionNumber,
    InvalidTransition {
        from: ObjectVersionStatus,
        to: ObjectVersionStatus,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> ObjectVersion {
        ObjectVersion::new(ObjectVersionId::new(), TenantId::new(), ObjectId::new(), 1)
            .expect("version number is positive")
    }

    #[test]
    fn version_lifecycle_is_draft_published_superseded() {
        let cases = [
            (
                ObjectVersionStatus::Draft,
                ObjectVersionStatus::Published,
                true,
            ),
            (
                ObjectVersionStatus::Published,
                ObjectVersionStatus::Superseded,
                true,
            ),
            (
                ObjectVersionStatus::Draft,
                ObjectVersionStatus::Superseded,
                false,
            ),
            (
                ObjectVersionStatus::Published,
                ObjectVersionStatus::Draft,
                false,
            ),
            (
                ObjectVersionStatus::Superseded,
                ObjectVersionStatus::Published,
                false,
            ),
        ];

        for (from, to, expected) in cases {
            assert_eq!(from.can_transition_to(to), expected, "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn version_number_must_be_positive() {
        assert_eq!(
            ObjectVersion::new(ObjectVersionId::new(), TenantId::new(), ObjectId::new(), 0,),
            Err(ObjectVersionError::InvalidVersionNumber)
        );
    }

    #[test]
    fn published_version_can_only_be_superseded() {
        let mut version = draft();
        version.publish().expect("draft can be published");
        version.supersede().expect("published can be superseded");

        assert!(version.status().is_terminal());
        assert_eq!(
            version.supersede(),
            Err(ObjectVersionError::InvalidTransition {
                from: ObjectVersionStatus::Superseded,
                to: ObjectVersionStatus::Superseded,
            })
        );
    }

    #[test]
    fn draft_cannot_skip_publication() {
        let mut version = draft();
        assert_eq!(
            version.supersede(),
            Err(ObjectVersionError::InvalidTransition {
                from: ObjectVersionStatus::Draft,
                to: ObjectVersionStatus::Superseded,
            })
        );
    }
}
