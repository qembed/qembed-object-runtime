//! Server-derived identity for unauthenticated public Object invocation.

use std::fmt;

use crate::ids::{ObjectId, TenantId};

/// Trusted edge context for one Anonymous subject invoking one public Object.
///
/// The fingerprint must be produced by the HTTP edge from the direct peer
/// address and a server-held keyed secret. It is intentionally opaque and is
/// never deserialized from a request DTO.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AnonymousContext {
    tenant_id: TenantId,
    object_id: ObjectId,
    subject_fingerprint: [u8; 32],
}

impl AnonymousContext {
    /// Creates an Anonymous context from a fingerprint already derived by the
    /// trusted server edge. This type is not a Principal and carries no
    /// authenticated user identity.
    pub fn from_server_fingerprint(
        tenant_id: TenantId,
        object_id: ObjectId,
        subject_fingerprint: [u8; 32],
    ) -> Result<Self, AnonymousContextError> {
        if tenant_id.as_uuid().is_nil() {
            return Err(AnonymousContextError::NilTenantId);
        }
        if object_id.as_uuid().is_nil() {
            return Err(AnonymousContextError::NilObjectId);
        }

        Ok(Self {
            tenant_id,
            object_id,
            subject_fingerprint,
        })
    }

    /// Returns the tenant bound to the public Object.
    #[must_use]
    pub const fn tenant_id(self) -> TenantId {
        self.tenant_id
    }

    /// Returns the Object being invoked.
    #[must_use]
    pub const fn object_id(self) -> ObjectId {
        self.object_id
    }

    /// Returns the opaque server-derived subject fingerprint.
    #[must_use]
    pub const fn subject_fingerprint(self) -> [u8; 32] {
        self.subject_fingerprint
    }
}

/// Invalid trusted Anonymous context input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnonymousContextError {
    NilTenantId,
    NilObjectId,
}

impl fmt::Display for AnonymousContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NilTenantId => formatter.write_str("Anonymous tenant ID must not be nil"),
            Self::NilObjectId => formatter.write_str("Anonymous Object ID must not be nil"),
        }
    }
}

impl std::error::Error for AnonymousContextError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_context_keeps_tenant_object_and_fingerprint_together() {
        let tenant_id = TenantId::new();
        let object_id = ObjectId::new();
        let fingerprint = [0x5a; 32];

        let context = AnonymousContext::from_server_fingerprint(tenant_id, object_id, fingerprint)
            .expect("trusted non-nil identifiers should be accepted");

        assert_eq!(context.tenant_id(), tenant_id);
        assert_eq!(context.object_id(), object_id);
        assert_eq!(context.subject_fingerprint(), fingerprint);
    }

    #[test]
    fn context_rejects_nil_tenant_and_object_ids() {
        assert_eq!(
            AnonymousContext::from_server_fingerprint(
                TenantId::from_uuid(uuid::Uuid::nil()),
                ObjectId::new(),
                [1; 32],
            ),
            Err(AnonymousContextError::NilTenantId)
        );
        assert_eq!(
            AnonymousContext::from_server_fingerprint(
                TenantId::new(),
                ObjectId::from_uuid(uuid::Uuid::nil()),
                [1; 32],
            ),
            Err(AnonymousContextError::NilObjectId)
        );
    }
}
