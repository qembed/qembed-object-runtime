# Owner API HTTP authentication and inspection

## Authentication

Owner routes use the following HTTP contract:

- `Authorization: Bearer <owner-token>`
- `X-Tenant-ID: <tenant-uuid>`

The tenant header is a selector for the credential lookup, not an authenticated identity. The API passes it to the persisted owner-token authenticator. Only after successful verification does `authenticate_owner` construct the non-deserializable Principal and trusted Scope. Request DTOs never supply a Principal or Scope.

Unknown, disabled, revoked, tenant-mismatched, and non-owner credentials are intentionally indistinguishable at the HTTP boundary and return a generic 401. Database failures return a generic 500. No credential or database diagnostic is included in responses.

## Invocation inspection

`GET /owner/invocations/{invocation_id}` returns the existing minimized invocation projection. A missing invocation and an invocation belonging to another tenant both return the same 404 response. Internal persistence state, provider metadata, lease details, trace IDs, and raw diagnostics are not exposed.

The HTTP handler parses headers and the path identifier, authenticates, and delegates to the invocation query use case. SQL and tenant scoping remain in the Store adapter.

## Scope

This is the first Owner API transport increment, not completion of RUST-044. Create object/version, publish, invoke, and full PostgreSQL API integration coverage remain outstanding. It does not start RUST-045.
