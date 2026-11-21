//! Security-sensitive policy capabilities.
//!
//! This module owns the only minting boundary for commands that have passed
//! domain authorization. Callers must never construct an AllowedCommand from
//! transport data, model output, or an unverified action.

use crate::command_id::derive_command_id;
use crate::ids::{CommandId, InvocationId};

/// An unforgeable-by-callers capability representing one policy-approved command.
///
/// The fields are private, the type is not serializable or cloneable, and its
/// only constructor is private to this module. A future domain Authorizer must
/// mint this value only after evaluating the proposed action and its context.
///
/// External code cannot construct a capability directly:
///
/// ```compile_fail
/// use qembed_domain::ids::{CommandId, InvocationId};
/// use qembed_domain::policy::AllowedCommand;
///
/// let _ = AllowedCommand {
///     command_id: CommandId::new(),
///     invocation_id: InvocationId::new(),
///     action_hash: [0; 32],
/// };
/// ```
///
/// The capability cannot be cloned:
///
/// ```compile_fail
/// use qembed_domain::policy::AllowedCommand;
///
/// fn require_clone<T: Clone>() {}
/// require_clone::<AllowedCommand>();
/// ```
///
/// It cannot be serialized:
///
/// ```compile_fail
/// use qembed_domain::policy::AllowedCommand;
///
/// fn require_serialize<T: serde::Serialize>() {}
/// require_serialize::<AllowedCommand>();
/// ```
///
/// It cannot be deserialized:
///
/// ```compile_fail
/// use qembed_domain::policy::AllowedCommand;
///
/// fn require_deserialize<T: serde::de::DeserializeOwned>() {}
/// require_deserialize::<AllowedCommand>();
/// ```
///
/// It has no default value:
///
/// ```compile_fail
/// use qembed_domain::policy::AllowedCommand;
///
/// fn require_default<T: Default>() {}
/// require_default::<AllowedCommand>();
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct AllowedCommand {
    command_id: CommandId,
    invocation_id: InvocationId,
    step_index: u32,
    action_hash: [u8; 32],
}

impl AllowedCommand {
    /// Returns the stable identity assigned to this approved command.
    #[must_use]
    pub const fn command_id(&self) -> CommandId {
        self.command_id
    }

    /// Returns the invocation for which this command was authorized.
    #[must_use]
    pub const fn invocation_id(&self) -> InvocationId {
        self.invocation_id
    }

    /// Returns the zero-based step index within the invocation.
    #[must_use]
    pub const fn step_index(&self) -> u32 {
        self.step_index
    }

    /// Returns the digest of the exact action that was authorized.
    #[must_use]
    pub const fn action_hash(&self) -> &[u8; 32] {
        &self.action_hash
    }

    /// Mints a capability after policy has approved the exact command context.
    ///
    /// Kept private so this module remains the sole minting boundary. The
    /// Authorizer introduced later must call this only after a positive policy
    /// decision; adapters and executors receive, but cannot manufacture, it.
    #[allow(dead_code)] // The Authorizer will use this mint boundary in RUST-078.
    fn mint(invocation_id: InvocationId, step_index: u32, action_hash: [u8; 32]) -> Self {
        Self {
            command_id: derive_command_id(invocation_id, step_index, &action_hash),
            invocation_id,
            step_index,
            action_hash,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_mint_creates_a_capability_bound_to_exact_command_context() {
        let invocation_id = InvocationId::new();
        let step_index = 4;
        let action_hash = [0x5a; 32];

        let allowed = AllowedCommand::mint(invocation_id, step_index, action_hash);

        assert_eq!(
            allowed.command_id(),
            derive_command_id(invocation_id, step_index, &action_hash)
        );
        assert_eq!(allowed.invocation_id(), invocation_id);
        assert_eq!(allowed.step_index(), step_index);
        assert_eq!(allowed.action_hash(), &action_hash);
    }
}
