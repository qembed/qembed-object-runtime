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

## Increment B — authenticated object creation

- Route: `POST /owner/objects`.
- Request body: empty; client-supplied object IDs and tenant identity are not accepted.
- Authentication: the approved Bearer token + `X-Tenant-ID` contract above. The tenant header is only an untrusted credential-lookup selector.
- The runtime `ObjectCreationService` generates the `ObjectId` and constructs a new Draft aggregate using the authenticated Scope's tenant.
- The Store persists the Draft object through its tenant-scoped transaction; the API handler does not access Store or SQLx.
- Response: `201 Created` with only `object_id` and `state: "draft"`.
- PostgreSQL API integration coverage verifies valid Owner creation, missing credentials, tenant/token mismatch, rejection of client-supplied body data, and persisted Draft state.

## Increment C — authenticated Draft version and snapshot creation

- Route: `POST /owner/objects/{object_id}/versions`.
- Request: `schema_version` is a positive PostgreSQL-INT-compatible integer; `config` is any valid JSON value.
- The original request is checked by both the DTO deserializer and the duplicate-key-rejecting JCS parser. This preserves integer-only DTO semantics while rejecting duplicate keys anywhere in the envelope. Duplicate keys, malformed JSON, unknown envelope fields, and invalid schema versions return 400.
- The `config` value is canonicalized to RFC 8785 bytes. Those exact bytes are the immutable snapshot source of truth and the basis of SHA-256 content identity.
- The runtime use case constructs a Draft version/snapshot pair. The Store allocates `version_number` while holding the object row lock and commits version + snapshot atomically.
- The endpoint deliberately does not run semantic validation or create validation evidence. A semantically incomplete but valid JSON config may be saved as Draft; publication remains blocked until trusted validation succeeds.
- Exact canonical content already present for the same tenant/object returns `200 OK` with `outcome: "identical_content"` and the existing IDs. New content returns `201 Created` with `outcome: "created"`.
- A missing or cross-tenant object returns the same 404. Invalid credentials remain generic 401; Store failures fail closed as generic 500.
- `schema_version` is the snapshot's schema identifier, not `object_versions.version_number`. No Schema v1 completeness or unknown-field policy is inferred.
- PostgreSQL API integration coverage verifies persisted Draft state, exact canonical bytes, deduplication, cross-tenant denial, tenant/token mismatch, and duplicate-key rejection.

## Remaining RUST-044 increments

- Create draft version/snapshot through the existing aggregate constructors and atomic Store operation.
- Publish only after trusted validation evidence is produced for the exact persisted snapshot; never accept evidence from an HTTP DTO.
- Invoke through the existing atomic begin-invocation contract after the approved request DTO and caller-selection contract are wired.
- Add PostgreSQL-backed API integration tests for every route, authentication failures, cross-tenant isolation, DTO validation, publication evidence binding, and stale-pointer conflicts.

No semantic rules beyond the documented bounded validator are introduced here. RUST-045 remains blocked until every RUST-044 increment and its acceptance criteria are complete.
