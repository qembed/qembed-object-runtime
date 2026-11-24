# RUST-052a — Durable JetStream pull consumer and ACK-after-commit

## Responsibilities

- JetStream owns durable pull delivery, consumer coordination and redelivery after an unacknowledged delivery.
- The NATS adapter receives messages from an already provisioned durable pull consumer. It does not create streams or consumers.
- The broker-neutral inbound message carries the stable Outbox `delivery_id`, subject and JSON payload. The publisher writes the delivery identity both as JetStream's `Nats-Msg-Id` deduplication key and as the explicit `Qembed-Delivery-Id` application header.
- The handler owns business processing and PostgreSQL idempotency. It may return success only after the transaction that records the processing result has committed.
- The consumer sends an explicit, confirmed JetStream ACK only after the handler returns success.

## Delivery contract

- Semantics remain at-least-once. A crash after PostgreSQL commit and before the ACK can cause redelivery.
- Redelivery must be harmless through durable PostgreSQL idempotency keyed by `delivery_id` and the relevant business identity (for Invocation work, `invocation_id`).
- A handler error or malformed message is left unacknowledged. JetStream may redeliver it after AckWait. RUST-052b adds bounded retry/backoff and durable poison-message parking/DLQ behavior.
- ACK confirmation failure is ambiguous. The message may be redelivered; the handler must remain idempotent.
- A broker ACK is not proof that the business operation is complete beyond the handler's durable commit.

## Trust boundary

- Message tenant/caller fields are not authorization. The Worker must recover tenant and trusted execution context from PostgreSQL under its service identity.
- The consumer adapter does not deserialize a client-provided tenant identity into a `Scope`.
- The transport adapter remains in `qembed-api`; runtime/domain contracts contain no async-nats SDK types.

## Validation

- Integration tests use a real NATS JetStream server and PostgreSQL 19.
- A failing handler must leave the message unacknowledged for redelivery.
- The successful handler commits a PostgreSQL marker before the consumer confirms the ACK.


## RUST-063 duplicate-delivery concurrency evidence

- `duplicate_jetstream_delivery_after_commit_has_one_postgres_effect` uses a real durable JetStream pull consumer and PostgreSQL. The first handler execution commits its business marker, then deliberately returns a retryable error to leave the message unacknowledged. JetStream redelivers the same stable delivery ID; the second execution commits idempotently and ACKs. The test asserts two handler executions but exactly one durable PostgreSQL effect.


## RUST-063 concurrency suite coverage

The CI suite combines the existing PostgreSQL integration cases with the new real-broker duplicate-delivery case:

- stale_compare_and_set_conflicts_across_independent_sessions opens two tenant transactions, verifies distinct pg_backend_pid() values, commits the first state transition, and rejects the stale second transition.
- invocation_lease_reclaim_fences_the_previous_worker_across_sessions uses a second one-connection worker pool; it verifies live-lease exclusion, expired-lease reclaim, monotonic fencing/version advance, and rejection of the former owner's writes.
- concurrent_budget_reservations_cannot_overspend_object_daily_limit releases two independent invocation tasks through a barrier and proves exactly one can reserve the final daily budget.
- independent_relay_processes_claim_disjoint_rows_and_publish_both starts two child relay processes, each with its own PostgreSQL pool and JetStream publisher, and proves disjoint claims and successful publication to the real broker.
- duplicate_jetstream_delivery_after_commit_has_one_postgres_effect commits a PostgreSQL business marker before deliberately returning a retryable handler error. Real JetStream redelivers the same delivery; the second processing attempt ACKs, while the durable business marker remains unique.

The CI PostgreSQL harness starts the nats:2.11-alpine JetStream service in compose.test.yaml; the workspace integration suite runs against that service. No broker mock is used for these delivery and relay scenarios.

- The concurrent budget reservation case now provisions two separate one-connection pools and asserts distinct pg_backend_pid() values before releasing both transactions through the barrier.

The RUST-063 changes are validated with the repository's pinned Rust 1.100.0 toolchain and the PostgreSQL 19 + JetStream CI environment.


## RUST-064 failure-injection evidence

See [RUST-064 M1b failure-injection matrix](RUST-064_FAILURE_INJECTION.md) for the crash-point matrix, expected PostgreSQL/JetStream states and recovery paths. The duplicate-delivery test now recreates the same durable consumer after the first PostgreSQL commit and before redelivery, explicitly covering worker restart at the ACK boundary.
