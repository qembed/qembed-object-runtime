# RUST-047 — M1a failure-injection coverage

## Scope

These PostgreSQL integration tests exercise the durable invocation boundaries with the deterministic MockChatProvider. They do not introduce a production dispatcher, process supervisor, or external provider integration.

## Scenarios

1. **Backend termination before begin-invocation commit.** A test-only trigger pauses the transaction on its final outbox_messages insert. The test identifies that exact PostgreSQL backend through a unique application_name and terminates it from a second connection. After the SQLx task returns, the test verifies that the idempotency claim, Invocation, user message, event, outbox message, budget reservation, provider attempt, and both usage-counter rows are all absent. This proves the begin-invocation write set rolls back atomically when the connection dies before COMMIT.
2. **Process teardown after commit.** The second scenario commits begin-invocation, closes the first pool, creates a fresh pool, reapplies migrations, and reconstructs the authenticated Scope from persisted Owner credentials. The pending Invocation remains available to a fresh worker.
3. **Provider timeout.** A scripted MockChatProvider timeout is treated as ambiguous. The persisted state remains Processing + OutcomeUnknown; the budget reservation remains Reserved, and no assistant message is written.
4. **Worker restart after ambiguous timeout.** After the first worker's lease expires, a fresh pool and worker reclaim the Invocation. The worker observes the durable OutcomeUnknown provider attempt and does not call the configured success script. The Invocation remains unresolved and no assistant output is persisted.

## Evidence boundary

The tests use real PostgreSQL and only a deterministic mock provider. They prove transaction atomicity and durable recovery for this M1a path. They do not prove outbox dispatch/redelivery, production process supervision, provider-side idempotency, or reconciliation of an external provider's ambiguous outcome; those remain separate roadmap work.
