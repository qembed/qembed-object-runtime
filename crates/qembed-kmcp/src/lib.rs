//! Provider-neutral policy and adapter contracts for the RT-050 KMCP.
//! No HTTP, PostgreSQL, OIDC, KMS, HSM, filesystem or network dependency.
//! See docs/RT-050_KMCP_CORE.md and docs/RT-050_KMCP_ADAPTER_PORTS.md.
#![forbid(unsafe_code)]
pub mod lifecycle;
pub mod ports;
pub use lifecycle::{LifecycleAction, LifecycleDecision, LifecycleError, LifecycleRequest, LifecycleState, plan_transition};
pub use ports::{AdminAuthorizer, AdminCredential, AdminOperation, AdminPermission, ApplyOutcome, AuditEvent, AuditExportError, AuditLease, AuditOutboxExporter, AuthorizationError, AuthorizedAdmin, KeyProvider, KeyRegistry, OperationRecord, ProviderError, ProvisionedKey, RegistryError, RegistryKey};
