//! Provider-facing reconciliation adapter boundary and safe placeholder.
//!
//! Concrete upstream integrations should implement the domain
//! `ToolExecutionReconciler` contract and be wired by the composition root.
//! Until a provider is selected, the placeholder deliberately returns
//! `Unresolved`: absence of an integration is not evidence that a side effect
//! failed, and must never authorize an automatic retry.

use std::convert::Infallible;

use qembed_domain::tool_execution_reconciliation::{
    ToolExecutionReconciler, ToolExecutionReconciliationOutcome,
    ToolExecutionReconciliationRequest,
};

/// Safe placeholder used when no concrete upstream provider adapter is configured.
///
/// This adapter performs no network I/O and makes no claim about the external
/// side effect. The execution therefore remains `Unknown` and eligible for a
/// later reconciliation attempt once a real provider adapter is configured.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnresolvedToolExecutionReconciler;

impl ToolExecutionReconciler for UnresolvedToolExecutionReconciler {
    type Error = Infallible;

    async fn reconcile_tool_execution(
        &self,
        _request: ToolExecutionReconciliationRequest,
    ) -> Result<ToolExecutionReconciliationOutcome, Self::Error> {
        Ok(ToolExecutionReconciliationOutcome::Unresolved)
    }
}

#[cfg(test)]
mod tests {
    use qembed_domain::{
        ids::{CommandId, InvocationId, ToolExecutionId},
        tool_execution::ToolExecutionFencingToken,
        tool_execution_reconciliation::{
            ToolExecutionReconciler, ToolExecutionReconciliationOutcome,
            ToolExecutionReconciliationRequest,
        },
    };

    use super::UnresolvedToolExecutionReconciler;

    #[tokio::test]
    async fn placeholder_never_infers_an_external_outcome() {
        let request = ToolExecutionReconciliationRequest::new(
            ToolExecutionId::new(),
            InvocationId::new(),
            CommandId::new(),
            7,
            ToolExecutionFencingToken::from_value(3),
        );

        let outcome = UnresolvedToolExecutionReconciler
            .reconcile_tool_execution(request)
            .await
            .expect("placeholder is infallible");

        assert_eq!(outcome, ToolExecutionReconciliationOutcome::Unresolved);
    }
}
