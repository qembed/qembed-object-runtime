# Invocation worker lifecycle

RUST-037 implements the first durable, provider-neutral worker path. It deliberately does not implement lease acquisition or reclaim; those are RUST-038.

## Durable input

`begin_invocation` now accepts the original user message and a separate absolute retention deadline. The user message is inserted into `interaction_messages` in the same transaction as the invocation, idempotency reservation, event, and outbox row.

A matching idempotency replay returns before inserting another message. The input text is not reconstructed from `payload_hash`; the hash remains opaque request identity material.

## State progression

`Received → ContextLoading → Processing → Verifying → Completed`

Each transition reloads the persisted Invocation and uses the domain state machine plus PostgreSQL optimistic version/fencing-token predicates.

- `Received → ContextLoading` and `ContextLoading → Processing` are independent committed CAS transitions.
- Provider I/O happens only while no SQL transaction is open.
- The provider response is inserted as one `assistant` interaction message in the same transaction as `Processing → Verifying`.
- `Verifying → Completed` records `finished_at` and advances the version. The assistant message already exists durably, so a restarted worker can complete without calling the provider again.
- Provider errors map to a small allowlist of stable error codes and are persisted with `Failed` and `finished_at`.
- A terminal invocation is read-only to this worker. A completed result is loaded from `interaction_messages`, not process memory.

The assistant message inherits the user message's retention deadline. Raw provider errors, SDK details, and credentials are not stored in `error_code`.

## Restart behavior and limits

A process restart at `Received` or `ContextLoading` resumes from the last committed state. A restart at `Verifying` loads the persisted assistant output and performs only the terminal transition. A restart after `Completed` returns the durable assistant output.

If a process dies after a provider response arrives but before the response transaction commits, the database remains in `Processing`; a later worker pass may call the provider again. The mock-first chat completion path has no external write side effect, but duplicate provider cost is possible. RUST-037 does not claim exactly-once provider execution.

There is no lease owner, heartbeat, lease expiry, or reclaim in this task. Two workers may both reach the provider while the row is `Processing`; only one can commit the response/status CAS. RUST-038 adds lease ownership and fencing so a stale worker cannot commit after reclaim.

## Persistence boundary

`qembed-domain::InvocationWorkStore` describes durable input/output and terminal transitions. `qembed-store::PgExecutionStore` implements it using tenant-scoped PostgreSQL transactions. `qembed-runtime::InvocationWorker` coordinates the domain ports and `ChatProvider`; it contains no SQLx or provider SDK types.

The worker does not yet dispatch the outbox, expose HTTP endpoints, execute tools, settle budgets, or implement cancellation. Those remain later roadmap tasks.