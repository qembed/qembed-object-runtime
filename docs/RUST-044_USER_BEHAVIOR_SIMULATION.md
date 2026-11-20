# RUST-044 — user behavior simulation (implementation-grounded)

**Purpose:** describe how an Owner/client can use the currently implemented HTTP API and what durable effects the Rust/SQL path produces. This is a behavioral walkthrough, not a claim that a full product UI, provisioning API, worker execution, or end-to-end M1a flow is already complete.

**Sources of truth:** `ROADMAP_RUST_v1.md`, `docs/RUST-044_OWNER_API_IMPLEMENTATION.md`, Owner handlers/composition, Runtime use cases, Store repositories/migrations, and the canonical schema/acceptance contract in `qembed/qembed-sql`. The canonical SQL repository is not modified by this document.

## 1. Actors and identity

- **Tenant Owner** authenticates with `Authorization: Bearer <owner-token>` and selects a tenant using `X-Tenant-ID: <tenant-uuid>`.
- The tenant header is an untrusted selector. Rust verifies the persisted credential before constructing the trusted `Principal`/`Scope`.
- **Caller** identifies the party on whose behalf an invocation runs. It is not the Owner Principal. The request's `caller_id` is only a selection request; PostgreSQL resolves the exact active Principal→Caller delegation.
- **Runtime/worker** processes durable invocations after the request transaction commits. The HTTP invoke handler does not call the provider.

Owner credentials, Caller records, delegations, and object budget policy are provisioned outside the five Owner routes described below. The Owner API does not silently create them.

## 2. Happy path: create, configure, publish, invoke, inspect

### Step A — create an Object

Request:

```http
POST /owner/objects
Authorization: Bearer <owner-token>
X-Tenant-ID: <tenant-uuid>
```

Expected response: `201 Created`, with a server-generated `object_id` and `state: "draft"`.

Durable effect: a Draft Object is inserted under the authenticated tenant Scope. The client cannot choose the Object ID or tenant identity.

### Step B — create a version and immutable Snapshot

Request:

```http
POST /owner/objects/{object_id}/versions
Content-Type: application/json

{"schema_version":1,"config":{"limits":{"max_tokens_per_invocation":4096}}}
```

Expected response for new content: `201 Created`, `outcome: "created"`, with version and Snapshot IDs.

Rust canonicalizes the config using RFC 8785/JCS and calculates its content identity. Store commits the Draft version and immutable Snapshot together. The Snapshot stores exact canonical bytes, SHA-256 hash, and a JSONB query projection; the canonical bytes remain the source of truth.

If the same canonical content is submitted again for the same tenant/Object, the API returns `200 OK`, `outcome: "identical_content"`, and the existing IDs. It does not create another version/Snapshot or alter publication state. Different content creates a new Draft version.

A syntactically valid but semantically incomplete config can be saved as Draft. Draft creation is not publication validation.

### Step C — publish

Request:

```http
POST /owner/objects/{object_id}/versions/{version_id}/publish
```

Expected successful response: `200 OK`, status `published`, version/Snapshot IDs, validator version, and validation coverage.

The current bounded validator verifies only that `config.limits.max_tokens_per_invocation` is a positive JSON integer. Other specification areas are reported as not validated; success must not be interpreted as complete semantic validation.

The Runtime obtains opaque validation evidence bound to tenant/Object/version/Snapshot/content hash/validator version. Store checks this evidence and uses the expected published-version pointer under the Object row lock. Version status, Object pointer/lifecycle, event, and outbox are committed atomically.

A validation rejection returns `422`; a lifecycle or stale-pointer conflict returns `409`; missing/cross-tenant resources are not distinguished publicly.

### Step D — provision invocation prerequisites

Before Invoke, two independently managed prerequisites must exist:

1. A Caller record and an active delegation from the authenticated Owner Principal to that exact Caller in the same tenant.
2. An `object_budgets` row for the Object.

The current Owner endpoints do not create Callers, grant delegations, or configure object budgets. These are provisioning responsibilities outside this API slice. Invoke fails closed if the Caller is not delegated, the Object has no current published version, or required budget configuration is absent/invalid.

### Step E — start an Invocation

Request:

```http
POST /owner/objects/{object_id}/invocations
Content-Type: application/json

{
  "caller_id": "<explicit-caller-uuid>",
  "idempotency_key": "client-generated-stable-key",
  "input": {"question":"hello"}
}
```

Expected first response: `202 Accepted`, status `received`, and an `invocation_id`.

Runtime computes SHA-256 over the RFC 8785/JCS canonical representation of the input envelope. The client cannot supply the hash, Snapshot ID, InvocationPool, or retention deadlines. For this authenticated Owner path the server chooses `InvocationPool::Authenticated`.

Within one PostgreSQL transaction Store:
- resolves and locks the active Principal→Caller delegation;
- claims the idempotency key in the tenant/Object/effective-Caller namespace;
- for a new request, locks the Object row `FOR SHARE` and resolves its current published Snapshot;
- pins the current knowledge revision;
- reserves budget and usage counters;
- inserts the Invocation, user interaction message, event, and outbox delivery record;
- commits before any provider/network I/O.

The idempotency retention deadline is 24 hours and user-message retention is 30 days. The key remains occupied until cleanup deletes its idempotency row; merely passing `expires_at` does not make the key reusable.

### Step F — retry safely and inspect progress

If the client retries the same tenant/Object/Caller/key with the same canonical input, the API returns `202 Accepted` with the original Invocation ID. It does not create another Invocation, reservation, message, event, or outbox row.

If the same key is reused with different input, the request conflicts (`409`).

The client polls:

```http
GET /owner/invocations/{invocation_id}
```

The response is deliberately minimized: Invocation ID, public status, result, public error code, and degraded flag. It does not expose tenant/Caller/Snapshot IDs, trace ID, input, provider metadata, lease owner, fencing token, or internal optimistic version.

The current status projection maps Received→queued; active processing states→running; Completed/CompletedDegraded→succeeded; Failed→failed; Cancelled→cancelled. A missing ID and an ID belonging to another tenant both return the same 404.

## 3. Behavior scenarios and expected outcomes

| User action | Expected public behavior | Durable/architectural consequence |
|---|---|---|
| Create Object with valid Owner credentials | 201, Draft Object ID | Object is tenant-scoped; ID generated by Runtime |
| Send a body to create Object | 400 | No client-supplied identity accepted |
| Create same canonical config twice | 201 then 200 identical_content | One version/Snapshot pair only |
| Create valid JSON missing the budget invariant | Draft creation succeeds | Publish later rejects it; draft storage is not semantic approval |
| Publish with positive integer token cap | 200 plus bounded coverage report | Evidence-bound atomic pointer/version/event/outbox update |
| Publish with missing/zero/non-integer token cap | 422 | Version remains Draft; current published pointer is unchanged |
| Invoke without an active Caller delegation | 404 | Caller identity is not disclosed; no Invocation is created |
| Invoke an Object without a published version | Fail closed | No Snapshot can be pinned |
| Invoke without object budget provisioning | Fail closed | No unbounded invocation is admitted |
| Retry same key and same input | 202 with same Invocation ID | No duplicate durable side effects |
| Retry same key with changed input | 409 | Existing key claim remains associated with original payload |
| Inspect another tenant's Invocation | 404, same as unknown ID | Tenant isolation is preserved |
| Poll while status is Received | queued | Provider execution is asynchronous, not part of HTTP request |
| Poll after CompletedDegraded | succeeded + degraded=true | Partial/degraded result is distinguishable from normal success |

## 4. Concurrency and failure behavior that acceptance tests must prove

1. **Publish vs Invoke:** both operations synchronize on the Object row. Invoke must pin either the previously current published Snapshot (if it wins the row lock first) or the replacement Snapshot (if publication wins first), never a Draft or unrelated Snapshot.
2. **Rollback:** a failure after an idempotency claim or after any later insert must leave no partial Invocation, reservation, message, event, or outbox record.
3. **Idempotency race:** two concurrent identical requests using the same effective Caller/key/input produce one durable Invocation and one reservation; both callers receive the same ID.
4. **Delegation revocation race:** the resolver's row locks and transaction boundary must prevent a delegation from being revoked midway through an admitted invocation.
5. **Tenant isolation:** missing and cross-tenant Object/Invocation lookups remain indistinguishable at the API boundary.
6. **Inspection minimization:** no internal persistence, execution, or provider fields appear in the public response.

These are acceptance obligations, not assertions that all corresponding concurrency/failure tests have already been added. The test PR must provide real PostgreSQL coverage for the applicable cases.

## 5. Explicit current boundaries — do not simulate as implemented

- No Owner endpoint for Caller creation, delegation management, credential rotation, or object-budget provisioning.
- No provider execution is performed synchronously by `POST .../invocations`; it commits durable work and returns 202.
- The bounded validator does not establish full schema, tools, permissions, provider, or policy correctness.
- Outbox dispatch, retries, advanced worker recovery, budget settlement, cancellation orchestration, and the full create→publish→invoke→retrieve end-to-end gate belong to their roadmap stages; do not imply that the Owner API slice alone proves them.
- No SQL DDL or canonical SQL contract is changed by this simulation.
