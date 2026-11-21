# RUST-051b — PostgreSQL Outbox to NATS JetStream

## Responsibilities

- PostgreSQL remains the only source of truth for event identity, payload, delivery state and publication receipt.
- `PgOutboxDispatcher` claims committed Outbox rows in a short transaction using the RUST-051a lease/fencing contract.
- `OutboxRelay` converts the delivery projection into a broker-neutral `OutboundMessage` and calls the `MessagePublisher` port after the claim transaction has committed.
- `NatsJetStreamPublisher` is the concrete adapter. It sets `Nats-Msg-Id` to the stable `delivery_id`, publishes to the configured subject, and waits for the JetStream PubAck future.
- Only after PubAck does the Relay call `record_publication`. The PostgreSQL transaction stores `delivered_at`, stream, sequence and duplicate flag together while fencing by owner and claim token.

## Failure semantics

| Failure point | Durable state and recovery |
|---|---|
| Before claim commit | No claim is visible; another Relay may claim the row |
| After claim commit, before publish | Lease expires or is released; same Outbox row is reclaimed |
| Publish accepted, PubAck lost/timeout | Outcome is ambiguous; retry uses the same `Nats-Msg-Id` and logical delivery ID |
| PubAck received, before PostgreSQL receipt commit | Message may be published again after reclaim; JetStream deduplication is bounded |
| Receipt transaction commits | PostgreSQL records the transport acknowledgement and stops claiming that row |

Delivery is **at-least-once**, not exactly-once. JetStream's message-ID deduplication window is an optimization against duplicates, not a replacement for consumer idempotency. Consumers must deduplicate by `delivery_id` and relevant business identity. A PubAck means the stream accepted the message; it does not mean a worker completed the business operation.

## PostgreSQL receipt

The canonical schema stores the latest `publication_stream`, `publication_sequence`, and `publication_duplicate` on the Outbox row, and appends every acknowledged publication to the immutable `outbox_publication_receipts` ledger in the same transaction. A consistency constraint rejects partial latest receipts. Legacy rows that were delivered before PubAck receipts existed retain null publication metadata; no historical receipt is fabricated. The ledger lets later recovery replay the same logical delivery without overwriting prior PubAck history.

The dispatcher role can read and update only the delivery projection, lease/fencing fields, and publication receipt. It cannot read `events` or `event_id`.

## Configuration and operations

The adapter accepts an already configured JetStream context or connects to a NATS URL. It does not create streams or consumers. Deployment provisions the stream and grants the Relay publish-only permissions for the intended subject. API handlers do not perform Relay I/O; the composition root wires the PostgreSQL store and publisher into the runtime service.

The current implementation exposes a bounded `run_once` batch. A long-running supervisor, reconnect/backoff policy, crash-window tests against a real `nats-server`, and durable consumers remain follow-up work (RUST-051c / RUST-052).
