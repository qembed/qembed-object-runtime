//! Immutable global Tool definitions and versions.

use serde_json::Value;

use crate::ids::{ToolId, ToolVersionId};

/// Stable identity and display metadata for a globally registered tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDefinition {
    id: ToolId,
    name: String,
    description: String,
}

impl ToolDefinition {
    pub fn new(
        id: ToolId,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Self, ToolDefinitionError> {
        let name = name.into();
        let description = description.into();
        if name.trim().is_empty() {
            return Err(ToolDefinitionError::EmptyName);
        }
        if description.trim().is_empty() {
            return Err(ToolDefinitionError::EmptyDescription);
        }
        Ok(Self { id, name, description })
    }

    #[must_use]
    pub const fn id(&self) -> ToolId { self.id }

    #[must_use]
    pub fn name(&self) -> &str { &self.name }

    #[must_use]
    pub fn description(&self) -> &str { &self.description }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolDefinitionError {
    EmptyName,
    EmptyDescription,
}

/// Risk classification persisted by the canonical SQL tool registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolRiskLevel {
    R0,
    R1,
    R2,
    R3,
    R4,
}

impl ToolRiskLevel {
    #[must_use]
    pub const fn as_database_value(self) -> &'static str {
        match self {
            Self::R0 => "R0",
            Self::R1 => "R1",
            Self::R2 => "R2",
            Self::R3 => "R3",
            Self::R4 => "R4",
        }
    }

    pub fn from_database_value(value: &str) -> Result<Self, ToolVersionError> {
        match value {
            "R0" => Ok(Self::R0),
            "R1" => Ok(Self::R1),
            "R2" => Ok(Self::R2),
            "R3" => Ok(Self::R3),
            "R4" => Ok(Self::R4),
            _ => Err(ToolVersionError::InvalidRiskLevel),
        }
    }

    const fn requires_idempotency(self) -> bool {
        matches!(self, Self::R2 | Self::R3 | Self::R4)
    }
}

/// One immutable, globally registered implementation contract for a Tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolVersion {
    id: ToolVersionId,
    tool_id: ToolId,
    version: String,
    risk_level: ToolRiskLevel,
    input_schema: Value,
    output_schema: Value,
    timeout_ms: u32,
    idempotent: bool,
}

impl ToolVersion {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: ToolVersionId,
        tool_id: ToolId,
        version: impl Into<String>,
        risk_level: ToolRiskLevel,
        input_schema: Value,
        output_schema: Value,
        timeout_ms: u32,
        idempotent: bool,
    ) -> Result<Self, ToolVersionError> {
        let version = version.into();
        if version.trim().is_empty() {
            return Err(ToolVersionError::EmptyVersion);
        }
        if timeout_ms == 0 {
            return Err(ToolVersionError::InvalidTimeout);
        }
        if risk_level.requires_idempotency() && !idempotent {
            return Err(ToolVersionError::NonIdempotentWriteTool);
        }

        Ok(Self {
            id,
            tool_id,
            version,
            risk_level,
            input_schema,
            output_schema,
            timeout_ms,
            idempotent,
        })
    }

    #[must_use]
    pub const fn id(&self) -> ToolVersionId { self.id }

    #[must_use]
    pub const fn tool_id(&self) -> ToolId { self.tool_id }

    #[must_use]
    pub fn version(&self) -> &str { &self.version }

    #[must_use]
    pub const fn risk_level(&self) -> ToolRiskLevel { self.risk_level }

    #[must_use]
    pub fn input_schema(&self) -> &Value { &self.input_schema }

    #[must_use]
    pub fn output_schema(&self) -> &Value { &self.output_schema }

    #[must_use]
    pub const fn timeout_ms(&self) -> u32 { self.timeout_ms }

    #[must_use]
    pub const fn idempotent(&self) -> bool { self.idempotent }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolVersionError {
    EmptyVersion,
    InvalidTimeout,
    InvalidRiskLevel,
    NonIdempotentWriteTool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_capable_risk_levels_require_idempotency() {
        for risk in [ToolRiskLevel::R2, ToolRiskLevel::R3, ToolRiskLevel::R4] {
            assert_eq!(
                ToolVersion::new(
                    ToolVersionId::new(),
                    ToolId::new(),
                    "1",
                    risk,
                    serde_json::json!({}),
                    serde_json::json!({}),
                    1_000,
                    false,
                ),
                Err(ToolVersionError::NonIdempotentWriteTool)
            );
        }
    }

    #[test]
    fn read_only_risk_levels_may_be_non_idempotent() {
        for risk in [ToolRiskLevel::R0, ToolRiskLevel::R1] {
            assert!(ToolVersion::new(
                ToolVersionId::new(),
                ToolId::new(),
                "1",
                risk,
                serde_json::json!({}),
                serde_json::json!({}),
                1_000,
                false,
            ).is_ok());
        }
    }

    #[test]
    fn version_requires_name_and_positive_timeout() {
        let args = (
            ToolVersionId::new(),
            ToolId::new(),
            ToolRiskLevel::R0,
            serde_json::json!({}),
            serde_json::json!({}),
        );
        assert_eq!(
            ToolVersion::new(args.0, args.1, " ", args.2, args.3.clone(), args.4.clone(), 1, true),
            Err(ToolVersionError::EmptyVersion)
        );
        assert_eq!(
            ToolVersion::new(args.0, args.1, "1", args.2, args.3, args.4, 0, true),
            Err(ToolVersionError::InvalidTimeout)
        );
    }
}
