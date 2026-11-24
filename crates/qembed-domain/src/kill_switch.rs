//! Hierarchical kill-switch policy and evaluation.

use std::future::Future;

use crate::ids::{ObjectId, TenantId, ToolId};
use crate::scope::Scope;

/// The tenant, Object and Tool whose side effect is about to be attempted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KillSwitchContext {
    tenant_id: TenantId,
    object_id: ObjectId,
    tool_id: ToolId,
}

impl KillSwitchContext {
    #[must_use]
    pub const fn new(tenant_id: TenantId, object_id: ObjectId, tool_id: ToolId) -> Self {
        Self {
            tenant_id,
            object_id,
            tool_id,
        }
    }

    #[must_use]
    pub const fn tenant_id(self) -> TenantId {
        self.tenant_id
    }

    #[must_use]
    pub const fn object_id(self) -> ObjectId {
        self.object_id
    }

    #[must_use]
    pub const fn tool_id(self) -> ToolId {
        self.tool_id
    }
}

/// One persisted switch. A Tool switch may be global (tenant_id=None) or
/// tenant-specific; a global Tool switch is evaluated before a tenant Tool switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KillSwitchScope {
    Platform,
    Tenant {
        tenant_id: TenantId,
    },
    Object {
        tenant_id: TenantId,
        object_id: ObjectId,
    },
    Tool {
        tenant_id: Option<TenantId>,
        tool_id: ToolId,
    },
}

impl KillSwitchScope {
    fn precedence_rank(&self) -> u8 {
        match self {
            Self::Platform => 0,
            Self::Tenant { .. } => 1,
            Self::Object { .. } => 2,
            Self::Tool {
                tenant_id: None, ..
            } => 3,
            Self::Tool {
                tenant_id: Some(_), ..
            } => 4,
        }
    }

    fn matches(&self, context: KillSwitchContext) -> bool {
        match self {
            Self::Platform => true,
            Self::Tenant { tenant_id } => *tenant_id == context.tenant_id,
            Self::Object {
                tenant_id,
                object_id,
            } => *tenant_id == context.tenant_id && *object_id == context.object_id,
            Self::Tool { tenant_id, tool_id } => {
                tenant_id.is_none_or(|id| id == context.tenant_id) && *tool_id == context.tool_id
            }
        }
    }
}

/// A single switch state read from the authoritative Store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KillSwitch {
    scope: KillSwitchScope,
    enabled: bool,
    reason: Option<String>,
}

impl KillSwitch {
    #[must_use]
    pub fn new(scope: KillSwitchScope, enabled: bool, reason: Option<String>) -> Self {
        Self {
            scope,
            enabled,
            reason,
        }
    }

    #[must_use]
    pub fn scope(&self) -> &KillSwitchScope {
        &self.scope
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
}

/// Effective result. Missing switches are treated as enabled; a disabled
/// matching switch at any level is an unconditional deny.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KillSwitchDecision {
    Allowed,
    Blocked {
        scope: KillSwitchScope,
        reason: Option<String>,
    },
}

/// Evaluates matching switches in Platform → Tenant → Object → Tool order.
/// An enabled lower-level switch never overrides a disabled higher-level one.
#[must_use]
pub fn evaluate_kill_switches(
    context: KillSwitchContext,
    switches: &[KillSwitch],
) -> KillSwitchDecision {
    for rank in 0..=4 {
        if let Some(switch) = switches.iter().find(|switch| {
            switch.scope.precedence_rank() == rank
                && switch.scope.matches(context)
                && !switch.enabled
        }) {
            return KillSwitchDecision::Blocked {
                scope: switch.scope.clone(),
                reason: switch.reason.clone(),
            };
        }
    }
    KillSwitchDecision::Allowed
}

/// Read-only persistence port for switches relevant to one Tool action.
pub trait KillSwitchStore: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    fn load_matching_kill_switches(
        &self,
        scope: &Scope,
        context: KillSwitchContext,
    ) -> impl Future<Output = Result<Vec<KillSwitch>, Self::Error>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> KillSwitchContext {
        KillSwitchContext::new(TenantId::new(), ObjectId::new(), ToolId::new())
    }

    #[test]
    fn no_switches_means_allowed() {
        assert_eq!(
            evaluate_kill_switches(context(), &[]),
            KillSwitchDecision::Allowed
        );
    }

    #[test]
    fn disabled_platform_switch_cannot_be_overridden_by_lower_levels() {
        let context = context();
        let switches = vec![
            KillSwitch::new(KillSwitchScope::Platform, false, Some("maintenance".into())),
            KillSwitch::new(
                KillSwitchScope::Tenant {
                    tenant_id: context.tenant_id(),
                },
                true,
                None,
            ),
            KillSwitch::new(
                KillSwitchScope::Object {
                    tenant_id: context.tenant_id(),
                    object_id: context.object_id(),
                },
                true,
                None,
            ),
            KillSwitch::new(
                KillSwitchScope::Tool {
                    tenant_id: Some(context.tenant_id()),
                    tool_id: context.tool_id(),
                },
                true,
                None,
            ),
        ];
        assert_eq!(
            evaluate_kill_switches(context, &switches),
            KillSwitchDecision::Blocked {
                scope: KillSwitchScope::Platform,
                reason: Some("maintenance".into())
            }
        );
    }

    #[test]
    fn disabled_tenant_switch_precedes_enabled_object_and_tool_switches() {
        let context = context();
        let switches = vec![
            KillSwitch::new(
                KillSwitchScope::Tenant {
                    tenant_id: context.tenant_id(),
                },
                false,
                None,
            ),
            KillSwitch::new(
                KillSwitchScope::Object {
                    tenant_id: context.tenant_id(),
                    object_id: context.object_id(),
                },
                true,
                None,
            ),
            KillSwitch::new(
                KillSwitchScope::Tool {
                    tenant_id: Some(context.tenant_id()),
                    tool_id: context.tool_id(),
                },
                true,
                None,
            ),
        ];
        assert!(matches!(
            evaluate_kill_switches(context, &switches),
            KillSwitchDecision::Blocked {
                scope: KillSwitchScope::Tenant { .. },
                ..
            }
        ));
    }

    #[test]
    fn disabled_object_switch_precedes_enabled_tool_switch() {
        let context = context();
        let switches = vec![
            KillSwitch::new(
                KillSwitchScope::Object {
                    tenant_id: context.tenant_id(),
                    object_id: context.object_id(),
                },
                false,
                None,
            ),
            KillSwitch::new(
                KillSwitchScope::Tool {
                    tenant_id: Some(context.tenant_id()),
                    tool_id: context.tool_id(),
                },
                true,
                None,
            ),
        ];
        assert!(matches!(
            evaluate_kill_switches(context, &switches),
            KillSwitchDecision::Blocked {
                scope: KillSwitchScope::Object { .. },
                ..
            }
        ));
    }

    #[test]
    fn disabled_tool_switch_blocks_when_ancestors_are_not_disabled() {
        let context = context();
        let switches = vec![KillSwitch::new(
            KillSwitchScope::Tool {
                tenant_id: Some(context.tenant_id()),
                tool_id: context.tool_id(),
            },
            false,
            Some("tool disabled".into()),
        )];
        assert!(matches!(
            evaluate_kill_switches(context, &switches),
            KillSwitchDecision::Blocked {
                scope: KillSwitchScope::Tool { .. },
                ..
            }
        ));
    }

    #[test]
    fn global_tool_switch_is_evaluated_before_tenant_specific_tool_switch() {
        let context = context();
        let switches = vec![
            KillSwitch::new(
                KillSwitchScope::Tool {
                    tenant_id: Some(context.tenant_id()),
                    tool_id: context.tool_id(),
                },
                false,
                None,
            ),
            KillSwitch::new(
                KillSwitchScope::Tool {
                    tenant_id: None,
                    tool_id: context.tool_id(),
                },
                false,
                Some("global".into()),
            ),
        ];
        assert_eq!(
            evaluate_kill_switches(context, &switches),
            KillSwitchDecision::Blocked {
                scope: KillSwitchScope::Tool {
                    tenant_id: None,
                    tool_id: context.tool_id()
                },
                reason: Some("global".into()),
            }
        );
    }

    #[test]
    fn switches_for_other_tenants_objects_and_tools_are_ignored() {
        let context = context();
        let switches = vec![
            KillSwitch::new(
                KillSwitchScope::Tenant {
                    tenant_id: TenantId::new(),
                },
                false,
                None,
            ),
            KillSwitch::new(
                KillSwitchScope::Object {
                    tenant_id: context.tenant_id(),
                    object_id: ObjectId::new(),
                },
                false,
                None,
            ),
            KillSwitch::new(
                KillSwitchScope::Tool {
                    tenant_id: None,
                    tool_id: ToolId::new(),
                },
                false,
                None,
            ),
        ];
        assert_eq!(
            evaluate_kill_switches(context, &switches),
            KillSwitchDecision::Allowed
        );
    }
}
