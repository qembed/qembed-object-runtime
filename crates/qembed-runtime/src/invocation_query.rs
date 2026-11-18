//! Read-only invocation query use case.

use qembed_domain::ids::InvocationId;
use qembed_domain::invocation_read::InvocationReadModel;
use qembed_domain::ports::InvocationReadStore;
use qembed_domain::scope::Scope;

/// Coordinates a tenant-scoped invocation read without exposing persistence details.
pub struct InvocationQueryService<S> {
    store: S,
}

impl<S> InvocationQueryService<S> {
    #[must_use]
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

impl<S: InvocationReadStore> InvocationQueryService<S> {
    pub async fn get(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
    ) -> Result<Option<InvocationReadModel>, S::Error> {
        self.store.get_invocation(scope, invocation_id).await
    }
}
