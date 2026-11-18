//! Client-facing invocation query projection.

use qembed_domain::ids::InvocationId;
use qembed_domain::invocation::InvocationStatus;
use qembed_domain::ports::{InvocationCancellationOutcome, InvocationCancellationStore, InvocationReadStore};
use qembed_domain::scope::Scope;
use qembed_runtime::invocation_cancellation::InvocationCancellationService;
use qembed_runtime::invocation_query::InvocationQueryService;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationApiStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InvocationApiError {
    pub code: String,
}

/// Deliberately minimized response: no tenant/caller/snapshot IDs, trace data,
/// lease owner, fencing token, internal version, raw input, or provider metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InvocationResponse {
    pub invocation_id: String,
    pub status: InvocationApiStatus,
    pub result: Option<String>,
    pub error: Option<InvocationApiError>,
    pub degraded: bool,
}

/// Result of an invocation cancellation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CancelInvocationResponse {
    pub invocation_id: String,
    pub status: InvocationApiStatus,
    /// True only when this request changed a nonterminal invocation to Cancelled.
    pub cancellation_applied: bool,
}

/// POST /invocations/{id}/cancel application handler. Missing and cross-tenant
/// IDs return None; a different terminal state is returned unchanged.
pub async fn cancel_invocation<S: InvocationCancellationStore>(
    service: &InvocationCancellationService<S>,
    scope: &Scope,
    invocation_id: InvocationId,
) -> Result<Option<CancelInvocationResponse>, S::Error> {
    let outcome = service.cancel(scope, invocation_id).await?;
    Ok(cancellation_response(invocation_id, outcome))
}

fn cancellation_response(
    invocation_id: InvocationId,
    outcome: InvocationCancellationOutcome,
) -> Option<CancelInvocationResponse> {
    let (status, cancellation_applied) = match outcome {
        InvocationCancellationOutcome::Cancelled { newly_cancelled } => {
            (InvocationApiStatus::Cancelled, newly_cancelled)
        }
        InvocationCancellationOutcome::AlreadyTerminal(status) => (api_status(status), false),
        InvocationCancellationOutcome::NotFound => return None,
    };
    Some(CancelInvocationResponse {
        invocation_id: invocation_id.to_string(),
        status,
        cancellation_applied,
    })
}

/// GET /invocations/{id} application handler. HTTP routing/status-code wiring
/// is added with the API transport skeleton; this function owns the safe view.
pub async fn get_invocation<S: InvocationReadStore>(
    query: &InvocationQueryService<S>,
    scope: &Scope,
    invocation_id: InvocationId,
) -> Result<Option<InvocationResponse>, S::Error> {
    let Some(view) = query.get(scope, invocation_id).await? else {
        return Ok(None);
    };

    let (status, degraded) = api_status_with_degraded(view.status());

    Ok(Some(InvocationResponse {
        invocation_id: view.id().to_string(),
        status,
        result: view.result().map(str::to_owned),
        error: view.error_code().map(|code| InvocationApiError {
            code: code.to_owned(),
        }),
        degraded,
    }))
}

fn api_status(status: InvocationStatus) -> InvocationApiStatus {
    api_status_with_degraded(status).0
}

fn api_status_with_degraded(status: InvocationStatus) -> (InvocationApiStatus, bool) {
    match status {
        InvocationStatus::Received => (InvocationApiStatus::Queued, false),
        InvocationStatus::ContextLoading
        | InvocationStatus::Processing
        | InvocationStatus::ExecutingTool
        | InvocationStatus::Verifying
        | InvocationStatus::WaitingForInput
        | InvocationStatus::WaitingForApproval => (InvocationApiStatus::Running, false),
        InvocationStatus::Completed => (InvocationApiStatus::Succeeded, false),
        InvocationStatus::CompletedDegraded => (InvocationApiStatus::Succeeded, true),
        InvocationStatus::Failed => (InvocationApiStatus::Failed, false),
        InvocationStatus::Cancelled => (InvocationApiStatus::Cancelled, false),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use qembed_domain::ids::InvocationId;

    #[test]
    fn cancellation_response_distinguishes_applied_repeated_and_terminal_requests() {
        let id = InvocationId::new();

        let applied = cancellation_response(
            id,
            InvocationCancellationOutcome::Cancelled {
                newly_cancelled: true,
            },
        )
        .expect("new cancellation has a response");
        assert_eq!(applied.status, InvocationApiStatus::Cancelled);
        assert!(applied.cancellation_applied);

        let repeated = cancellation_response(
            id,
            InvocationCancellationOutcome::Cancelled {
                newly_cancelled: false,
            },
        )
        .expect("repeated cancellation has a response");
        assert_eq!(repeated.status, InvocationApiStatus::Cancelled);
        assert!(!repeated.cancellation_applied);

        let terminal = cancellation_response(
            id,
            InvocationCancellationOutcome::AlreadyTerminal(InvocationStatus::Failed),
        )
        .expect("terminal invocation has a response");
        assert_eq!(terminal.status, InvocationApiStatus::Failed);
        assert!(!terminal.cancellation_applied);

        assert!(cancellation_response(id, InvocationCancellationOutcome::NotFound).is_none());
    }

    #[test]
    fn response_serialization_exposes_only_the_client_contract() {
        let response = InvocationResponse {
            invocation_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            status: InvocationApiStatus::Succeeded,
            result: Some("answer".to_owned()),
            error: None,
            degraded: false,
        };
        let json = serde_json::to_value(response).expect("response serializes");
        let object = json.as_object().expect("response is an object");
        assert_eq!(object.len(), 5);
        assert_eq!(
            object.get("status").and_then(|value| value.as_str()),
            Some("succeeded")
        );
        for forbidden in [
            "tenant_id",
            "caller_id",
            "snapshot_id",
            "trace_id",
            "lease_owner",
            "fencing_token",
            "version",
            "input",
            "provider",
        ] {
            assert!(
                !object.contains_key(forbidden),
                "unexpected field {forbidden}"
            );
        }
    }
}
