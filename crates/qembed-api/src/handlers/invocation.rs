//! Client-facing invocation query projection.

use qembed_domain::ids::InvocationId;
use qembed_domain::invocation::InvocationStatus;
use qembed_domain::ports::InvocationReadStore;
use qembed_domain::scope::Scope;
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

    let (status, degraded) = match view.status() {
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
    };

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

#[cfg(test)]
mod tests {
    use super::*;

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
