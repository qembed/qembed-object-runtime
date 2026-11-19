# RUST-044 Owner API — implementation increments

The Owner API is delivered in bounded increments. Each increment remains part of RUST-044; none authorizes starting RUST-045.

## Approved HTTP authentication contract (2026-11-20)

- Owner credentials are sent as `Authorization: Bearer <owner-token>`.
- The tenant selector is sent as `X-Tenant-ID: <tenant-uuid>`.
- The tenant header is untrusted input used only to scope the credential lookup. It is never converted directly into a Principal or Scope.
- The trusted Owner Scope is created only after the persisted credential has been verified by `OwnerAuthenticator` and `authenticate_owner`.
- Invalid credentials and non-owner identities produce the same public 401 response. Database failures produce a generic 500 response; internal diagnostics are never returned.

## Increment A — authenticated invocation inspection

- Route: `GET /owner/invocations/{invocation_id}`.
- Authentication: the contract above.
- Query: tenant-scoped `InvocationQueryService` backed by `PgInvocationQueryStore`.
- Missing and cross-tenant invocation IDs both produce the same 404 response.
- The existing minimized `InvocationResponse` projection is retained; internal IDs, provider details, lease state, and raw diagnostics are not exposed.
- Transport validates headers and path syntax only. Runtime query service owns the use case; Store owns tenant-scoped SQL.

## Remaining RUST-044 increments

- Create object and create draft version/snapshot through the existing aggregate constructors and atomic Store operation.
- Publish only after trusted validation evidence is produced for the exact persisted snapshot; never accept evidence from an HTTP DTO.
- Invoke through the existing atomic begin-invocation contract after the approved request DTO and caller-selection contract are wired.
- Add PostgreSQL-backed API integration tests for every route, authentication failures, cross-tenant isolation, DTO validation, publication evidence binding, and stale-pointer conflicts.

No semantic rules beyond the documented bounded validator are introduced here. RUST-045 remains blocked until every RUST-044 increment and its acceptance criteria are complete.
