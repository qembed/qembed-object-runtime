# Invocation read API contract

RUST-039 adds the read-side application path for `GET /invocations/{id}`.
The HTTP router and transport status-code wiring are part of the later API
transport skeleton; this contract defines the query and client-safe response.

## Lookup and tenant isolation

The authenticated request supplies a trusted `Scope`. The query adapter
always binds both `scope.tenant_id()` and the requested invocation ID in the
same tenant-scoped PostgreSQL transaction. An invocation owned by another
tenant is indistinguishable from an unknown ID and returns no record.

## Public response

The response contains only:

- `invocation_id`
- `status`: `queued`, `running`, `succeeded`, `failed`, or `cancelled`
- `result`: assistant output, only after `Completed` or
  `CompletedDegraded`
- `error.code`: a stable allowlisted error code for `Failed`
- `degraded`: true only for `CompletedDegraded`

Internal lifecycle states map to `queued` (`Received`) or `running`
(`ContextLoading`, `Processing`, `ExecutingTool`, `Verifying`,
`WaitingForInput`, `WaitingForApproval`). Both successful terminal states
map to `succeeded`; the degraded flag preserves the distinction.

The read projection never returns user input, raw provider errors, credentials,
tenant/caller/snapshot identifiers, trace IDs, lease owner/deadline,
fencing token, optimistic version, or arbitrary database columns. Assistant
output persisted during `Verifying` remains hidden until the invocation is
terminal-successful.

## Error behavior

A missing invocation and a cross-tenant invocation both return the same
not-found result. Failed invocations expose only the stable error-code
allowlist already used by the worker; raw SDK/provider details are never
returned.
