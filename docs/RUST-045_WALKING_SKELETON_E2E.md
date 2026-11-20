# RUST-045 — walking-skeleton end-to-end test

## Coverage

`crates/qembed-api/tests/walking_skeleton_e2e.rs` exercises one durable flow against a real PostgreSQL database:

1. Authenticate a persisted Tenant Owner through the same Bearer token + `X-Tenant-ID` HTTP contract as the Owner API.
2. Create an Object over HTTP.
3. Create a Draft version and immutable Snapshot over HTTP.
4. Publish the version over HTTP using the bounded trusted validator.
5. Provision the explicit Caller delegation and Object budget as test fixtures. These remain outside the Owner API.
6. Start an Invocation over HTTP and retry the exact request with the same idempotency key.
7. Assert that the retry returns the original Invocation ID and that exactly one Invocation, idempotency claim, user message, outbox row, and budget reservation exist.
8. Tear down the first API router and close its PostgreSQL pool. Reconnect with a fresh pool, authenticate again, and construct a new Runtime worker and deterministic `MockChatProvider`.
9. Process the persisted Invocation after restart, persisting the provider response, assistant message, usage settlement, and terminal state in PostgreSQL.
10. Retrieve the result through the Owner HTTP inspection endpoint and verify that a different authenticated tenant receives 404.

The mock is scripted against the exact canonical user-message content and worker request, so the test has no provider SDK, credentials, or external network dependency.

## Restart and dispatch boundary

The test simulates a process restart between HTTP acceptance and worker pickup by dropping the first API router, closing the first pool, and creating a fresh pool, router, worker, and provider in the second phase. The Invocation and its outbox record survive only in PostgreSQL.

The test invokes the worker with the persisted Invocation ID directly. This deliberately does **not** claim that an outbox dispatcher or production process supervisor exists; those are separate lifecycle capabilities. The acceptance criterion here is durable work surviving process teardown and completing through the Runtime worker after reconnection.

## Acceptance assertions

- Same idempotency key + same canonical input returns the same Invocation ID.
- Duplicate admission creates no duplicate durable side effects.
- The restarted worker obtains the result from the persisted Invocation input and writes the result to PostgreSQL.
- Owner inspection returns `succeeded`, the deterministic mock result, and `degraded: false`.
- Cross-tenant inspection is denied with the same not-found boundary.
- The terminal Invocation, provider-call attempt, and settled token usage are present in PostgreSQL.

The test runs in the existing PostgreSQL integration-test CI job. No schema/DDL changes, external provider, Connect/gRPC transport, or MSRV change are introduced.
