# RUST-052c — PostgreSQL recovery and durable Outbox replay

## Responsibilities

- PostgreSQL remains the recovery source of truth; NATS is a transport, not the durable invocation ledger.
- The recovery sweeper runs under a trusted tenant Scope and asks PostgreSQL to mark eligible original Outbox rows for replay.
- The SQL function selects only stale Received invocations, with no active lease, no cancellation request, a delivered original Outbox row, and explicit safe provider-attempt evidence.
- Missing provider-attempt evidence fails closed. MayHaveBeenSent, OutcomeUnknown, and ResponseReceived are not replay candidates.
- An invocation handler must commit terminal Failed state before returning a Permanent failure for invocation.received. That keeps a poison message already parked in DLQ out of the Received-only recovery set.
- Waiting-for-input/approval and all non-Received invocation states are not auto-replayed by this queue recovery path.
- The progress deadline is derived from COALESCE(heartbeat_at, started_at); the current schema has no separate progress_deadline column.

## Durable replay contract

- The marker is stored on the existing outbox_messages row.
- Replay preserves the original delivery_id, topic, and payload.
- The Outbox Relay can claim a delivered row only while its replay marker is present.
- The marker remains set across Relay publish failures and lease expiry.
- A successful new PubAck updates the current receipt projection and clears the pending marker in the same PostgreSQL transaction.
- Every PubAck, including replays, appends to outbox_publication_receipts; previous receipt history is never updated or deleted.
- JetStream deduplication is only a bounded transport optimization. Consumers still use the original delivery_id for idempotency.

## Concurrency and safety

- The SQL sweeper locks candidate Invocation and Outbox rows with FOR UPDATE SKIP LOCKED.
- The recovery function is owned by qembed_definer; the runtime receives EXECUTE permission and supplies tenant context transaction-locally.
- The dispatcher sees only the delivery projection plus replay marker fields; it does not gain access to the event log or invocation rows.
- Only queued Received work is replayed. In-progress work is recovered through invocation lease/fencing logic, not by bypassing its state machine. Permanent handler failures must already have moved the invocation to Failed; WaitingForInput, WaitingForApproval, and terminal states are not replayed.
- Provider outcomes that may have been accepted are never automatically replayed. They require reconciliation.

## Validation

- PostgreSQL integration: stale Received row is marked, fresh/live or Unknown work is not.
- Outbox Relay claims the marked, already-delivered row with the same delivery ID.
- Successful replay clears the marker and appends a second PubAck receipt without deleting the first.
- Repeated sweeps do not create duplicate pending markers.
- Concurrency tests use independent PostgreSQL sessions.
