# Error handling contract

This document defines the error boundary for the Rust runtime and the public API.
It is normative for error classification and response safety. The implementation
lives in qembed_runtime::error and qembed_api::errors.

## Responsibility boundaries

1. **Adapters** classify implementation-specific failures. SQLx errors, provider
   SDK errors, raw response bodies, connection strings, SQL statements, and
   vendor request identifiers remain inside their adapter/diagnostic boundary.
2. **Runtime** expresses the semantic outcome using RuntimeError. It does not
   depend on HTTP status codes, SQLx, or provider SDK types.
3. **API** maps RuntimeError to a stable HTTP status and a deliberately small
   public body. Public messages are fixed constants; never build them from
   Display, Debug, or an adapter error string.

A known semantic failure should be classified before reaching the generic
fallback. An unclassified Store/adapter error maps to internal_error (500),
not to a guessed client error.

## Public mapping

| Runtime category | HTTP | Public code | Meaning |
|---|---:|---|---|
| InvalidRequest | 400 | invalid_request | Request violates a public input contract |
| Unauthenticated | 401 | unauthorized | Credentials are absent or rejected |
| Forbidden | 403 | forbidden | Authenticated principal lacks permission |
| NotFound | 404 | not_found | Resource is absent or intentionally concealed by scope |
| Conflict | 409 | conflict | Request conflicts with persisted state or idempotency |
| BudgetExceeded | 429 | budget_exceeded | A configured Qembed usage budget rejected the operation |
| RateLimited | 429 | rate_limited | An upstream provider explicitly rate-limited the request |
| DependencyUnavailable | 503 | dependency_unavailable | A required dependency timed out or is unavailable |
| ProviderFailure | 502 | provider_error | Provider response/failure did not satisfy the provider contract |
| Internal | 500 | internal_error | Unexpected or unclassified internal failure |

The response body is:

~~~json
{
  "code": "internal_error",
  "message": "An unexpected internal error occurred."
}
~~~

The code values and fixed messages are compatibility-sensitive API contract.
Renaming a Rust enum variant must not silently rename a public code. Add/update
explicit mapping tests when the public contract is intentionally changed.

## Classification rules

- A malformed client DTO or invalid domain request maps to InvalidRequest.
- Authentication rejection maps to Unauthenticated; an authenticated
  principal that lacks an authorization grant maps to Forbidden.
- Missing resources and resources outside the caller's tenant scope must be
  indistinguishable where the resource lookup contract requires concealment.
  Do not return a cross-tenant existence signal.
- Idempotency payload mismatch and invalid state transitions map to Conflict,
  not Internal, once the application boundary has positively identified those
  semantic cases.
- Explicit budget rejection maps to BudgetExceeded. Do not infer it from a
  generic database error.
- Provider rate-limit responses map to RateLimited; an optional provider
  retry-after hint is internal mapping metadata and is not currently serialized
  into the public body.
- Provider timeout or provider unavailability maps to DependencyUnavailable.
  Malformed provider output and unclassified provider failures map to
  ProviderFailure.
- Unknown SQLx, storage, or adapter failures map to Internal. Never parse SQL
  error text or constraint names to construct public messages.

## Sensitive information that must not cross the API boundary

Never include the following in an error response:

- SQL text, PostgreSQL diagnostics, constraint names, schema/table/column names;
- SQLx error strings, connection strings, hostnames, credentials, or DSNs;
- provider SDK type names, raw provider messages/bodies, API keys, request
  headers, vendor request IDs, or model-routing secrets;
- stack traces, Rust Debug output, source chains, filesystem paths;
- tenant IDs, principal IDs, caller IDs, internal invocation metadata, lease
  owners, fencing tokens, or raw user input unless a separate response contract
  explicitly authorizes that data.

The generic mapper accepts an error only to discard it and return a fixed
internal_error. Detailed diagnostics belong in protected server-side
observability with appropriate redaction and access controls. Do not log
credentials, raw provider payloads, or secrets.

## Retry and invocation outcome safety

An HTTP status is not permission to retry a provider call. In particular,
timeouts, connection resets, cancellation after the pre-send marker, and worker
crashes can represent an ambiguous provider outcome. Preserve the durable
provider-call reconciliation rules: OutcomeUnknown must not be treated as zero
usage, a confirmed rejection, or permission for an automatic retry.

A 429 or 503 response does not by itself prove that an invocation was not
accepted upstream. Clients should use the invocation/idempotency contract and
durable status retrieval rather than creating a new logical operation blindly.

## Testing requirements

- Unit tests cover every RuntimeError to HTTP/code mapping.
- Provider-neutral domain errors are tested at the conversion boundary.
- Serialization tests assert the exact public body shape and fixed messages.
- Secret-bearing synthetic internal errors must map to the same generic 500
  response without serializing their text.
- When the HTTP transport is introduced, API integration tests must assert
  actual status codes and response bodies; these mapping tests do not claim
  that an HTTP server/router already exists.
