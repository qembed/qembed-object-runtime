# Invocation worker lifecycle

RUST-037 implements the first durable, provider-neutral worker path. It deliberately does not implement lease acquisition or reclaim; those are RUST-038.

## Durable input

`begin_invocation` now accepts the original user message and a separate absolute retention deadline. The user message is inserted into `interaction_messages` in the same transaction as the invocation, idempotency reservation, event, and outbox row.

A matching idempotency replay returns before inserting another message. The input text is not reconstructed from `payload_hash`; the hash remains opaque request identity material.

## State progression

`Received → ContextLoading → Processing → Verifying → Completed`

Each transition reloads the persisted Invocation and uses the domain state machine plus PostgreSQL optimistic version/fencing-token predicates.

- `Received → ContextLoading` and `ContextLoading → Processing` are independent committed CAS transitions.
- Unowned compare-and-set cannot enter `Processing`, `ExecutingTool`, or `Verifying`; those execution states require the lease-fenced store contract.
- A `provider_call_attempts` row is created with the budget reservation. Before provider I/O, the worker commits `NotStarted → MayHaveBeenSent` under its live lease; if that write fails, it does not send.
- Provider I/O happens only while no SQL transaction is open.
- The provider response, normalized usage (nullable only when unavailable), assistant message, `ResponseReceived`, and `Processing → Verifying` commit in one tenant-scoped transaction.
- `Verifying → Completed` first performs replay-safe token settlement from the durable usage, then records `finished_at` and advances the version. Missing usage settles the original non-zero estimate and completes as `CompletedDegraded`.
- Any generic provider error after the pre-send marker is ambiguous. The worker records `OutcomeUnknown`, does not fail the invocation, does not release the reservation, and never automatically sends that call again.
- A terminal invocation is read-only to this worker. A completed result is loaded from `interaction_messages`, not process memory.

The assistant message inherits the user message's retention deadline. Raw provider errors, SDK details, and credentials are not stored in `error_code`.

## Cancellation

Cancellation locks the durable provider attempt before changing invocation
state, serializing against the worker's pre-send marker. Before the marker,
`NotStarted` becomes `CancelledBeforeSend` and the invocation
can be cancelled safely. After `MayHaveBeenSent`, cancellation records
`OutcomeUnknown`; the invocation lease is revoked, but its token
reservation remains held because the provider may have accepted the request.
A durable `ResponseReceived` must finish settlement rather than be
discarded by cancellation.

## Restart behavior and limits

A restart at `Received` or `ContextLoading` resumes from the
last committed state. At `Processing`, the worker reads the durable
provider attempt before deciding what to do. Only `NotStarted` permits
the first send. `MayHaveBeenSent` is conservatively reconciled to
`OutcomeUnknown`; `OutcomeUnknown` is never reset to
`NotStarted` and never triggers an automatic provider retry.
`ResponseReceived` is committed atomically with the assistant message
and `Verifying`, so recovery settles usage and completes without
another provider call.

The database state is deliberately conservative: a crash after the marker but
before the socket write is still treated as potentially accepted. Generic
timeouts, resets, cancellation, worker crashes, HTTP 5xx, and unclassified
errors do not prove non-acceptance. Only a provider adapter with an explicit,
documented guarantee may eventually classify a result as
`ConfirmedNotAccepted`.

## Persistence boundary

`qembed-domain::InvocationWorkStore` describes durable input/output and terminal transitions. `qembed-store::PgExecutionStore` implements it using tenant-scoped PostgreSQL transactions. `qembed-runtime::InvocationWorker` coordinates the domain ports and `ChatProvider`; it contains no SQLx or provider SDK types.

The worker does not yet dispatch the outbox, expose HTTP endpoints, execute tools, or reconcile ambiguous provider calls automatically. Provider-side idempotency and safe deduplication remain deferred until a route-specific guarantee is verified.