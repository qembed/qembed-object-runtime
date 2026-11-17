//! Object aggregate and lifecycle state machine.

use crate::ids::{ObjectId, ObjectVersionId, TenantId};

/// Lifecycle states of an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectState {
    Draft,
    Validating,
    Active,
    Suspended,
    Archived,
}

impl ObjectState {
    /// Returns whether the lifecycle permits a transition to `next`.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Draft, Self::Validating)
                | (Self::Validating, Self::Draft)
                | (Self::Validating, Self::Active)
                | (Self::Active, Self::Suspended)
                | (Self::Suspended, Self::Active)
                | (Self::Active, Self::Archived)
                | (Self::Suspended, Self::Archived)
        )
    }

    /// Whether this state is terminal.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Archived)
    }
}

/// An object and the version currently published by it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    id: ObjectId,
    tenant_id: TenantId,
    state: ObjectState,
    published_version_id: Option<ObjectVersionId>,
}

impl Object {
    /// Creates a new object in the Draft state.
    #[must_use]
    pub const fn new(id: ObjectId, tenant_id: TenantId) -> Self {
        Self {
            id,
            tenant_id,
            state: ObjectState::Draft,
            published_version_id: None,
        }
    }

    /// Reconstitutes an object and checks persisted-state invariants.
    pub fn from_persisted(
        id: ObjectId,
        tenant_id: TenantId,
        state: ObjectState,
        published_version_id: Option<ObjectVersionId>,
    ) -> Result<Self, ObjectError> {
        if matches!(state, ObjectState::Active | ObjectState::Suspended)
            && published_version_id.is_none()
        {
            return Err(ObjectError::PublishedVersionRequired);
        }

        Ok(Self {
            id,
            tenant_id,
            state,
            published_version_id,
        })
    }

    #[must_use]
    pub const fn id(&self) -> ObjectId {
        self.id
    }

    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    #[must_use]
    pub const fn state(&self) -> ObjectState {
        self.state
    }

    #[must_use]
    pub const fn published_version_id(&self) -> Option<ObjectVersionId> {
        self.published_version_id
    }

    /// Starts object validation.
    pub fn begin_validation(&mut self) -> Result<(), ObjectError> {
        self.transition_to(ObjectState::Validating)
    }

    /// Returns a rejected validation attempt to Draft.
    pub fn reject_validation(&mut self) -> Result<(), ObjectError> {
        self.transition_to(ObjectState::Draft)
    }

    /// Activates a validated object with its first published version.
    pub fn activate(
        &mut self,
        published_version_id: ObjectVersionId,
    ) -> Result<(), ObjectError> {
        if self.state != ObjectState::Validating {
            return Err(ObjectError::InvalidTransition {
                from: self.state,
                to: ObjectState::Active,
            });
        }

        self.published_version_id = Some(published_version_id);
        self.state = ObjectState::Active;
        Ok(())
    }

    /// Applies a lifecycle transition that does not require additional data.
    pub fn transition_to(&mut self, next: ObjectState) -> Result<(), ObjectError> {
        if self.state == ObjectState::Validating && next == ObjectState::Active {
            return Err(ObjectError::PublishedVersionRequired);
        }

        if !self.state.can_transition_to(next) {
            return Err(ObjectError::InvalidTransition {
                from: self.state,
                to: next,
            });
        }

        self.state = next;
        Ok(())
    }

    /// Replaces the published version without changing the object's lifecycle state.
    ///
    /// The repository must perform the corresponding version transitions and
    /// pointer update atomically in PostgreSQL.
    pub fn replace_published_version(
        &mut self,
        expected_current: ObjectVersionId,
        next: ObjectVersionId,
    ) -> Result<(), ObjectError> {
        if !matches!(self.state, ObjectState::Active | ObjectState::Suspended) {
            return Err(ObjectError::CannotReplacePublishedVersion);
        }

        if self.published_version_id != Some(expected_current) {
            return Err(ObjectError::PublishedVersionMismatch);
        }

        self.published_version_id = Some(next);
        Ok(())
    }
}

/// Object lifecycle invariant or transition error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectError {
    InvalidTransition { from: ObjectState, to: ObjectState },
    PublishedVersionRequired,
    CannotReplacePublishedVersion,
    PublishedVersionMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ObjectVersionId;

    #[test]
    fn object_lifecycle_accepts_only_specified_transitions() {
        let cases = [
            (ObjectState::Draft, ObjectState::Validating, true),
            (ObjectState::Validating, ObjectState::Draft, true),
            (ObjectState::Validating, ObjectState::Active, true),
            (ObjectState::Active, ObjectState::Suspended, true),
            (ObjectState::Suspended, ObjectState::Active, true),
            (ObjectState::Active, ObjectState::Archived, true),
            (ObjectState::Suspended, ObjectState::Archived, true),
            (ObjectState::Draft, ObjectState::Active, false),
            (ObjectState::Draft, ObjectState::Archived, false),
            (ObjectState::Archived, ObjectState::Draft, false),
            (ObjectState::Archived, ObjectState::Active, false),
        ];

        for (from, to, expected) in cases {
            assert_eq!(from.can_transition_to(to), expected, "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn activation_requires_a_published_version() {
        let mut object = Object::new(ObjectId::new(), TenantId::new());
        object.begin_validation().expect("Draft can validate");

        assert_eq!(
            object.transition_to(ObjectState::Active),
            Err(ObjectError::PublishedVersionRequired)
        );

        let version_id = ObjectVersionId::new();
        object.activate(version_id).expect("validated object can activate");
        assert_eq!(object.state(), ObjectState::Active);
        assert_eq!(object.published_version_id(), Some(version_id));
    }

    #[test]
    fn active_and_suspended_objects_require_a_published_version_when_loaded() {
        let result = Object::from_persisted(
            ObjectId::new(),
            TenantId::new(),
            ObjectState::Active,
            None,
        );

        assert_eq!(result, Err(ObjectError::PublishedVersionRequired));
    }

    #[test]
    fn archived_is_terminal() {
        let mut object = Object::new(ObjectId::new(), TenantId::new());
        object.begin_validation().expect("Draft can validate");
        object
            .activate(ObjectVersionId::new())
            .expect("validated object can activate");
        object
            .transition_to(ObjectState::Archived)
            .expect("active object can be archived");

        assert!(object.state().is_terminal());
        assert!(object.transition_to(ObjectState::Draft).is_err());
    }

    #[test]
    fn published_version_replacement_is_compare_and_set() {
        let current = ObjectVersionId::new();
        let next = ObjectVersionId::new();
        let mut object = Object::from_persisted(
            ObjectId::new(),
            TenantId::new(),
            ObjectState::Active,
            Some(current),
        )
        .expect("active object requires a published version");

        assert_eq!(
            object.replace_published_version(ObjectVersionId::new(), next),
            Err(ObjectError::PublishedVersionMismatch)
        );
        object
            .replace_published_version(current, next)
            .expect("matching published pointer can be replaced");
        assert_eq!(object.published_version_id(), Some(next));
    }
}
