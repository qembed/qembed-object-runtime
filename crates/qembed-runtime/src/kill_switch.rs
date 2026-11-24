//! Runtime gate for hierarchical kill-switch evaluation.

use std::fmt;
use std::future::Future;

use qembed_domain::kill_switch::{
    KillSwitchAuthority, KillSwitchContext, KillSwitchDecision, KillSwitchScope, KillSwitchStore,
    evaluate_kill_switches,
};

/// Error returned when a switch prevents an action or its state cannot be read.
#[derive(Debug)]
pub enum KillSwitchServiceError<E> {
    Store(E),
    ContextTenantMismatch,
    Disabled {
        scope: KillSwitchScope,
        reason: Option<String>,
    },
}

impl<E: fmt::Display> fmt::Display for KillSwitchServiceError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "kill-switch lookup failed: {error}"),
            Self::ContextTenantMismatch => {
                formatter.write_str("kill-switch authority does not match the requested Tenant/Object binding")
            }
            Self::Disabled { scope, reason } => write!(
                formatter,
                "action blocked by {scope:?} kill switch: {}",
                reason.as_deref().unwrap_or("disabled")
            ),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for KillSwitchServiceError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

/// Checks the authoritative switch state immediately before running a side effect.
pub struct KillSwitchService<S> {
    store: S,
}

impl<S> KillSwitchService<S>
where
    S: KillSwitchStore,
{
    #[must_use]
    pub const fn new(store: S) -> Self {
        Self { store }
    }

    pub async fn ensure_enabled(
        &self,
        authority: KillSwitchAuthority<'_>,
        context: KillSwitchContext,
    ) -> Result<(), KillSwitchServiceError<S::Error>> {
        let authority_matches = match authority {
            KillSwitchAuthority::Principal(scope) => scope.tenant_id() == context.tenant_id(),
            KillSwitchAuthority::Anonymous(anonymous) => {
                anonymous.tenant_id() == context.tenant_id()
                    && anonymous.object_id() == context.object_id()
            }
        };
        if !authority_matches {
            return Err(KillSwitchServiceError::ContextTenantMismatch);
        }
        let switches = self
            .store
            .load_matching_kill_switches(authority, context)
            .await
            .map_err(KillSwitchServiceError::Store)?;
        match evaluate_kill_switches(context, &switches) {
            KillSwitchDecision::Allowed => Ok(()),
            KillSwitchDecision::Blocked { scope, reason } => {
                Err(KillSwitchServiceError::Disabled { scope, reason })
            }
        }
    }

    /// The closure is invoked only after the latest Store snapshot allows the action.
    pub async fn run_if_enabled<T, F, Fut>(
        &self,
        authority: KillSwitchAuthority<'_>,
        context: KillSwitchContext,
        side_effect: F,
    ) -> Result<T, KillSwitchServiceError<S::Error>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T> + Send,
        T: Send,
    {
        self.ensure_enabled(authority, context).await?;
        Ok(side_effect().await)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        future::{Future, ready},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use qembed_domain::{
        anonymous::AnonymousContext,
        ids::{ObjectId, TenantId, ToolId},
        kill_switch::{
            KillSwitch, KillSwitchAuthority, KillSwitchContext, KillSwitchScope, KillSwitchStore,
        },
        test_support::scope_for_tenant,
    };

    use super::{KillSwitchService, KillSwitchServiceError};

    struct MemorySwitchStore(Vec<KillSwitch>);

    impl KillSwitchStore for MemorySwitchStore {
        type Error = Infallible;

        fn load_matching_kill_switches(
            &self,
            _authority: KillSwitchAuthority<'_>,
            _context: KillSwitchContext,
        ) -> impl Future<Output = Result<Vec<KillSwitch>, Self::Error>> + Send {
            ready(Ok(self.0.clone()))
        }
    }

    #[tokio::test]
    async fn disabled_switch_prevents_side_effect_closure_from_running() {
        let tenant_id = TenantId::new();
        let scope = scope_for_tenant(tenant_id);
        let context = KillSwitchContext::new(tenant_id, ObjectId::new(), ToolId::new());
        let service = KillSwitchService::new(MemorySwitchStore(vec![KillSwitch::new(
            KillSwitchScope::Object {
                tenant_id,
                object_id: context.object_id(),
            },
            false,
            None,
        )]));
        let calls = AtomicUsize::new(0);

        let result = service
            .run_if_enabled(KillSwitchAuthority::Principal(&scope), context, || async {
                calls.fetch_add(1, Ordering::SeqCst);
            })
            .await;

        assert!(matches!(
            result,
            Err(KillSwitchServiceError::Disabled { .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn enabled_hierarchy_runs_side_effect_once() {
        let tenant_id = TenantId::new();
        let scope = scope_for_tenant(tenant_id);
        let context = KillSwitchContext::new(tenant_id, ObjectId::new(), ToolId::new());
        let service = KillSwitchService::new(MemorySwitchStore(vec![
            KillSwitch::new(KillSwitchScope::Platform, true, None),
            KillSwitch::new(KillSwitchScope::Tenant { tenant_id }, true, None),
            KillSwitch::new(
                KillSwitchScope::Object {
                    tenant_id,
                    object_id: context.object_id(),
                },
                true,
                None,
            ),
            KillSwitch::new(
                KillSwitchScope::Tool {
                    tenant_id: Some(tenant_id),
                    tool_id: context.tool_id(),
                },
                true,
                None,
            ),
        ]));
        let calls = AtomicUsize::new(0);

        service
            .run_if_enabled(KillSwitchAuthority::Principal(&scope), context, || async {
                calls.fetch_add(1, Ordering::SeqCst);
            })
            .await
            .expect("enabled hierarchy should permit the side effect");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn anonymous_qctx2_authority_is_supported_without_synthetic_principal() {
        let tenant_id = TenantId::new();
        let object_id = ObjectId::new();
        let anonymous = AnonymousContext::from_server_fingerprint(tenant_id, object_id, [0x5a; 32])
            .expect("server-derived Anonymous context should be valid");
        let context = KillSwitchContext::new(tenant_id, object_id, ToolId::new());
        let service = KillSwitchService::new(MemorySwitchStore(vec![KillSwitch::new(
            KillSwitchScope::Tenant { tenant_id },
            false,
            None,
        )]));
        let calls = AtomicUsize::new(0);

        let result = service
            .run_if_enabled(
                KillSwitchAuthority::Anonymous(&anonymous),
                context,
                || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await;

        assert!(matches!(
            result,
            Err(KillSwitchServiceError::Disabled { .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn context_from_another_tenant_fails_before_store_or_side_effect() {
        let tenant_id = TenantId::new();
        let scope = scope_for_tenant(tenant_id);
        let context = KillSwitchContext::new(TenantId::new(), ObjectId::new(), ToolId::new());
        let service = KillSwitchService::new(MemorySwitchStore(vec![]));
        let calls = AtomicUsize::new(0);

        let result = service
            .run_if_enabled(KillSwitchAuthority::Principal(&scope), context, || async {
                calls.fetch_add(1, Ordering::SeqCst);
            })
            .await;

        assert!(matches!(
            result,
            Err(KillSwitchServiceError::ContextTenantMismatch)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
