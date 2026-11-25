//! Domain types and contracts shared across the Qembed workspace.
//!
//! # Security-sensitive compile-fail contracts
//!
//! These examples are compiled as part of `cargo test -p qembed-domain --doc`.
//! They keep the trust boundaries of the public domain API executable: code
//! outside this crate must not manufacture authenticated identity, tenant
//! scope, or an authorization capability.
//!
//! External callers cannot construct a `Principal` from fields:
//!
//! ```compile_fail
//! use qembed_domain::ids::PrincipalId;
//! use qembed_domain::principal::{Principal, PrincipalKind};
//!
//! let _ = Principal {
//!     principal_id: PrincipalId::new(),
//!     kind: PrincipalKind::Authenticated,
//! };
//! ```
//!
//! External callers cannot invoke the trusted-authentication constructor:
//!
//! ```compile_fail
//! use qembed_domain::ids::PrincipalId;
//! use qembed_domain::principal::{Principal, PrincipalKind};
//!
//! let _ = Principal::from_verified_auth(PrincipalId::new(), PrincipalKind::TenantOwner);
//! ```
//!
//! A `Principal` cannot be reconstructed from serialized input:
//!
//! ```compile_fail
//! use qembed_domain::principal::Principal;
//!
//! fn require_deserialize<T: serde::de::DeserializeOwned>() {}
//! require_deserialize::<Principal>();
//! ```
//!
//! External callers cannot construct a `Scope` from fields:
//!
//! ```compile_fail
//! use qembed_domain::ids::TenantId;
//! use qembed_domain::scope::Scope;
//!
//! let _ = Scope { tenant_id: TenantId::new(), principal: todo!() };
//! ```
//!
//! External callers cannot invoke the trusted-scope constructor:
//!
//! ```compile_fail
//! use qembed_domain::ids::TenantId;
//! use qembed_domain::scope::Scope;
//!
//! let _ = Scope::from_verified_auth(TenantId::new(), todo!());
//! ```
//!
//! A `Scope` cannot be reconstructed from serialized input:
//!
//! ```compile_fail
//! use qembed_domain::scope::Scope;
//!
//! fn require_deserialize<T: serde::de::DeserializeOwned>() {}
//! require_deserialize::<Scope>();
//! ```
//!
//! External callers cannot manufacture an `AllowedCommand`:
//!
//! ```compile_fail
//! use qembed_domain::ids::{CommandId, InvocationId};
//! use qembed_domain::policy::AllowedCommand;
//!
//! let _ = AllowedCommand {
//!     command_id: CommandId::new(),
//!     invocation_id: InvocationId::new(),
//!     action_hash: [0; 32],
//! };
//! ```
//!
//! An `AllowedCommand` cannot be cloned:
//!
//! ```compile_fail
//! use qembed_domain::policy::AllowedCommand;
//!
//! fn require_clone<T: Clone>() {}
//! require_clone::<AllowedCommand>();
//! ```
//!
//! An `AllowedCommand` cannot be serialized or deserialized:
//!
//! ```compile_fail
//! use qembed_domain::policy::AllowedCommand;
//!
//! fn require_serialize<T: serde::Serialize>() {}
//! fn require_deserialize<T: serde::de::DeserializeOwned>() {}
//! require_serialize::<AllowedCommand>();
//! require_deserialize::<AllowedCommand>();
//! ```
//!
//! An `AllowedCommand` has no default value:
//!
//! ```compile_fail
//! use qembed_domain::policy::AllowedCommand;
//!
//! fn require_default<T: Default>() {}
//! require_default::<AllowedCommand>();
//! ```

pub mod anonymous;
pub mod auth;
pub mod budget;
pub mod caller;
pub mod chat;
pub mod command_id;
pub mod ids;
pub mod invocation;
pub mod invocation_lease;
pub mod invocation_read;
pub mod jcs;
pub mod kill_switch;
pub mod knowledge;
pub mod knowledge_ingestion;
pub mod object;
pub mod object_validation;
pub mod object_version;
pub mod policy;
pub mod ports;
pub mod principal;
pub mod provider_call_attempt;
pub mod rate_limit;
pub mod scope;
pub mod snapshot;
pub mod tool_execution;
pub mod tool_execution_lease;
pub mod tool_execution_reconciliation;

/// Test-only constructors for integration tests in adapter crates.
///
/// This module is available only when the `test-support` Cargo feature is
/// enabled. Production builds must not enable that feature.
#[cfg(feature = "test-support")]
pub mod test_support {
    use crate::ids::{PrincipalId, TenantId};
    use crate::principal::{Principal, PrincipalKind};
    use crate::scope::Scope;

    /// Creates a scope for a test using the same trusted constructors as the
    /// authentication boundary.
    #[must_use]
    pub fn scope_for_tenant(tenant_id: TenantId) -> Scope {
        let principal =
            Principal::from_verified_auth(PrincipalId::new(), PrincipalKind::TenantOwner)
                .expect("a generated principal ID must be valid");

        Scope::from_verified_auth(tenant_id, principal)
            .expect("a generated tenant ID must be valid")
    }
}
