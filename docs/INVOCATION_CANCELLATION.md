# Invocation cancellation contract

RUST-040 adds a tenant-scoped application path for cancelling an invocation.
The HTTP router and transport status-code wiring remain part of RUST-044.

## Semantics

- A nonterminal invocation is atomically moved to `Cancelled`.
- The transaction records `cancel_requested_at` and `finished_at`, advances
  the optimistic version and fencing token, and releases the worker lease.
- The fencing-token increment prevents a worker holding an older claim from
  persisting assistant output or completing the invocation after cancellation.
- Repeating cancellation is idempotent: the response remains `cancelled` and
  reports `cancellation_applied: false`.
- A previously completed or failed invocation is left unchanged and its
  existing terminal status is returned with `cancellation_applied: false`.
- Unknown and cross-tenant IDs both produce the same not-found application result.

## External side effects

Cancellation is a durable runtime state transition, not a distributed rollback.
If a provider request or another external side effect has already been accepted,
the runtime cannot promise to undo it. The worker may finish that external call,
but its stale lease/fencing token prevents the result from being committed to
the cancelled invocation.

## Transaction and concurrency

Cancellation and worker writes serialize on the invocation row. If a worker
commits a terminal state first, cancellation observes that terminal state and
does not overwrite it. If cancellation commits first, later writes from the
previous worker fail their owner/lease/version/fencing checks.
