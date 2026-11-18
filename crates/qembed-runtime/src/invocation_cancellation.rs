//! Durable invocation cancellation use case.

use qembed_domain::ids::InvocationId;
use qembed_domain::ports::{InvocationCancellationOutcome, InvocationCancellationStore};
use qembed_domain::scope::Scope;

/// Coordinates cancellation without depending on PostgreSQL or HTTP details.
pub struct InvocationCancellationService<S> {
    store: S,
}

impl<S> InvocationCancellationService<S> {
    #[must_use]
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

impl<S: InvocationCancellationStore> InvocationCancellationService<S> {
    pub async fn cancel(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
    ) -> Result<InvocationCancellationOutcome, S::Error> {
        self.store.cancel_invocation(scope, invocation_id).await
    }
}
