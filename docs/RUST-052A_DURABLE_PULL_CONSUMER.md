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
