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

## Draft version creation

`POST /owner/objects/{object_id}/versions` creates an immutable Draft version and snapshot.

Request:

```json
{
  "schema_version": 1,
  "config": {
    "limits": {
      "max_tokens_per_invocation": 4096
    }
  }
}
```

- `schema_version` must be a positive integer that fits PostgreSQL `INTEGER`. It is distinct from the server-assigned `version_number`.
- `config` may be any valid JSON value. The endpoint rejects duplicate JSON object keys and stores its RFC 8785 canonical bytes.
- Draft creation does not imply semantic validation. A semantically incomplete config may be stored as Draft; trusted publication validation remains a separate operation.
- New content returns 201 with `outcome: "created"`. Exact canonical content already stored for this tenant/object returns 200 with `outcome: "identical_content"` and the existing version/snapshot IDs.
- A missing or cross-tenant object returns the same 404. Invalid request JSON/DTO returns 400.

## Scope

This is a bounded Owner API transport increment, not completion of RUST-044. Publish, invoke, and the remaining end-to-end acceptance coverage are still outstanding. It does not start RUST-045.
