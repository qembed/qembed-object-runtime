//! Fail-closed validation gate for commands before an executor can see them.
use std::{error::Error, fmt};

use qembed_domain::{ids::ObjectId, policy::AllowedCommand, ports::ToolBindingStore, scope::Scope};

/// A command whose captured Tool binding was revalidated against PostgreSQL.
#[derive(Debug)]
pub struct ValidatedAllowedCommand {
    command: AllowedCommand,
}

impl ValidatedAllowedCommand {
    #[must_use]
    pub fn command(&self) -> &AllowedCommand {
        &self.command
    }
}

#[derive(Debug)]
pub enum BindingValidationError<E> {
    MissingBindingIdentity,
    StaleOrDisabledBinding,
    Store(E),
}

impl<E: fmt::Display> fmt::Display for BindingValidationError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingBindingIdentity => {
                f.write_str("Tool command has no captured binding identity")
            }
            Self::StaleOrDisabledBinding => {
                f.write_str("Tool binding changed or is disabled; execution rejected")
            }
            Self::Store(error) => write!(f, "Tool binding validation failed: {error}"),
        }
    }
}

impl<E: Error + 'static> Error for BindingValidationError<E> {}

/// Consumes the capability and returns an executor-eligible wrapper only when
/// the exact Tool ID, ToolVersion ID and monotonic binding version remain active.
pub async fn validate_allowed_command_binding<S: ToolBindingStore>(
    store: &S,
    scope: &Scope,
    object_id: ObjectId,
    command: AllowedCommand,
) -> Result<ValidatedAllowedCommand, BindingValidationError<S::Error>> {
    let binding = command
        .tool_binding()
        .ok_or(BindingValidationError::MissingBindingIdentity)?;
    let current = store
        .is_tool_binding_current(scope, object_id, binding)
        .await
        .map_err(BindingValidationError::Store)?;
    if !current {
        return Err(BindingValidationError::StaleOrDisabledBinding);
    }
    Ok(ValidatedAllowedCommand { command })
}
