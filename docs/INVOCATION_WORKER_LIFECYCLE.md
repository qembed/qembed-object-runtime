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

## Cancellation

RUST-040 adds a tenant-scoped cancellation operation. A nonterminal invocation
is durably moved to `Cancelled`; the transaction records
`cancel_requested_at` and `finished_at`, advances the version and fencing
token, and releases the worker lease. A stale worker can no longer heartbeat,
persist an assistant response, or complete the invocation.

Cancellation cannot roll back a provider request or other external side effect
that has already been accepted. The provider call may still finish; its result
is discarded because the cancelled invocation no longer accepts worker writes.

## Restart behavior and limits

A process restart at `Received` or `ContextLoading` resumes from the last committed state. A restart at `Verifying` loads the persisted assistant output and performs only the terminal transition. A restart after `Completed` returns the durable assistant output.

If a process dies after a provider response arrives but before the response transaction commits, the database remains in `Processing`; a later worker pass may call the provider again. The mock-first chat completion path has no external write side effect, but duplicate provider cost is possible. RUST-037 does not claim exactly-once provider execution.

RUST-038 adds PostgreSQL-backed lease ownership, heartbeat, expiry, and reclaim. A worker must claim before processing; every lifecycle write checks owner, unexpired lease, fencing token, and optimistic version. Reclaim increments token and version, so a stale worker cannot persist a response or terminal state. Heartbeats continue while waiting for provider I/O. The provider call itself may still complete after ownership is lost, and the result is then discarded.

## Retry guarantees

The durable invocation and transactional outbox record establish the work
before provider I/O. Lease expiry/reclaim permits another worker attempt, and
fencing prevents a stale attempt from persisting over the current owner. The
worker derives a provider idempotency key from the invocation ID and reuses it
across attempts; it never derives the key from the worker identity or fencing
token.

This is at-least-once processing/recovery, not exactly-once provider
execution. Actual outbox delivery remains the responsibility of the later
outbox-dispatcher work. Duplicate upstream execution is suppressed only when
the configured adapter advertises support and the provider honors the key
within its documented scope and retention window. Otherwise a crash after
provider success but before the response transaction commits can still incur
a second provider execution/cost.

## Persistence boundary

`qembed-domain::InvocationWorkStore` describes durable input/output and terminal transitions. `qembed-store::PgExecutionStore` implements it using tenant-scoped PostgreSQL transactions. `qembed-runtime::InvocationWorker` coordinates the domain ports and `ChatProvider`; it contains no SQLx or provider SDK types.

The worker does not yet dispatch the outbox, expose HTTP endpoints, execute tools, or settle budgets. Those remain later roadmap tasks.