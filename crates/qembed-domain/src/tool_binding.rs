//! Immutable identity of one Object-to-Tool binding observed during authorization.
use crate::ids::{ToolId, ToolVersionId};

/// Exact binding identity captured by an Allow decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ToolBindingIdentity {
    tool_id: ToolId,
    tool_version_id: ToolVersionId,
    binding_version: u64,
}

impl ToolBindingIdentity {
    /// Creates a binding identity. Version zero is reserved for an unbound Object.
    pub fn new(tool_id: ToolId, tool_version_id: ToolVersionId, binding_version: u64) -> Result<Self, ToolBindingIdentityError> {
        if binding_version == 0 {
            return Err(ToolBindingIdentityError::ZeroBindingVersion);
        }
        Ok(Self { tool_id, tool_version_id, binding_version })
    }
    #[must_use] pub const fn tool_id(self) -> ToolId { self.tool_id }
    #[must_use] pub const fn tool_version_id(self) -> ToolVersionId { self.tool_version_id }
    #[must_use] pub const fn binding_version(self) -> u64 { self.binding_version }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolBindingIdentityError { ZeroBindingVersion }
