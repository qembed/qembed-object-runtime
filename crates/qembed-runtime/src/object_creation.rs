//! Owner use case for creating a new Draft object.

use qembed_domain::ids::{ObjectId};
use qembed_domain::object::Object;
use qembed_domain::ports::ObjectCreationStore;
use qembed_domain::scope::Scope;

/// Creates tenant-owned objects through the persistence port.
#[derive(Clone, Debug)]
pub struct ObjectCreationService<S> {
    store: S,
}

impl<S: ObjectCreationStore> ObjectCreationService<S> {
    /// Creates the use case with its persistence adapter.
    #[must_use]
    pub const fn new(store: S) -> Self {
        Self { store }
    }

    /// Generates the object identity and persists a new Draft object.
    ///
    /// The tenant is taken exclusively from the authenticated Scope; callers
    /// cannot choose an object ID or tenant through transport input.
    pub async fn create(&self, scope: &Scope) -> Result<ObjectId, ObjectCreationError<S::Error>> {
        let object = Object::new(ObjectId::new(), scope.tenant_id());
        self.store
            .create_object(scope, &object)
            .await
            .map_err(ObjectCreationError::Store)?;
        Ok(object.id())
    }
}

/// Failure while executing the object-creation use case.
#[derive(Debug)]
pub enum ObjectCreationError<E> {
    Store(E),
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::future::{Future, ready};
    use std::sync::Mutex;

    use qembed_domain::ids::TenantId;
    use qembed_domain::object::{Object, ObjectState};
    use qembed_domain::ports::ObjectCreationStore;
    use qembed_domain::scope::Scope;
    use qembed_domain::test_support::scope_for_tenant;

    use super::*;

    #[derive(Default)]
    struct RecordingStore(Mutex<Vec<Object>>);

    impl ObjectCreationStore for RecordingStore {
        type Error = Infallible;

        fn create_object(
            &self,
            _: &Scope,
            object: &Object,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.0.lock().expect("recording store lock").push(object.clone());
            ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn create_generates_a_draft_object_for_the_authenticated_tenant() {
        let tenant_id = TenantId::new();
        let scope = scope_for_tenant(tenant_id);
        let service = ObjectCreationService::new(RecordingStore::default());

        let object_id = service.create(&scope).await.expect("creation succeeds");
        let objects = service.store.0.lock().expect("recording store lock");

        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].id(), object_id);
        assert_eq!(objects[0].tenant_id(), tenant_id);
        assert_eq!(objects[0].state(), ObjectState::Draft);
    }
}
