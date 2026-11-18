//! Client-safe read projection for invocation status queries.

use crate::ids::InvocationId;
use crate::invocation::InvocationStatus;

/// Read-only view; deliberately excludes tenant, caller, snapshot, trace,
/// lease owner, fencing token, optimistic version, and provider payload metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvocationReadModel {
    id: InvocationId,
    status: InvocationStatus,
    result: Option<String>,
    error_code: Option<String>,
}

impl InvocationReadModel {
    #[must_use]
    pub const fn new(
        id: InvocationId,
        status: InvocationStatus,
        result: Option<String>,
        error_code: Option<String>,
    ) -> Self {
        Self { id, status, result, error_code }
    }

    #[must_use]
    pub const fn id(&self) -> InvocationId { self.id }

    #[must_use]
    pub const fn status(&self) -> InvocationStatus { self.status }

    #[must_use]
    pub fn result(&self) -> Option<&str> { self.result.as_deref() }

    #[must_use]
    pub fn error_code(&self) -> Option<&str> { self.error_code.as_deref() }
}
