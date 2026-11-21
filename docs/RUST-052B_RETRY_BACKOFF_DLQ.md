# RUST-052b — Retry/backoff and poison-message DLQ

## Responsibilities

- The application handler classifies failures as retryable or permanent and supplies an error code plus a diagnostic message.
- The NATS consumer owns retry scheduling and dead-letter routing; business handlers do not call NATS APIs.
- Retryable failures use bounded delayed NAK with exponential backoff.
- Permanent failures are dead-lettered immediately. Retryable failures are dead-lettered after the configured maximum delivery attempt.
- The DLQ subject and stream are provisioned by deployment configuration; the adapter does not create or mutate streams.

## Delivery and loss-prevention contract

- A successful handler is ACKed only after its PostgreSQL transaction has committed.
- A retryable failure below the attempt limit receives NAK with a calculated delay.
- A permanent or exhausted failure is published to the DLQ with a confirmed JetStream PubAck before the source message receives a confirmed ACK.
- If DLQ publication fails, the source is not ACKed; it receives a delayed NAK so parking can be retried.
- The DLQ envelope preserves the original raw payload bytes, source stream/consumer/sequence, subject, delivery attempt, original delivery ID when present, and failure classification/diagnostics.
- The DLQ message ID is deterministic from source stream, consumer and stream sequence. JetStream deduplication suppresses nearby duplicate publications; because its deduplication window is finite, DLQ consumers must also be idempotent using the stable dead-letter ID.
- The original delivery ID is retained unchanged. DLQ identity is separate and never replaces the original business idempotency key.

## Retry policy

- max_attempts counts JetStream delivery attempts, including the current attempt.
- Backoff starts at initial_retry_delay, multiplies by retry_multiplier, and is capped at max_retry_delay.
- Permanent errors bypass retry delay and go directly to DLQ.
- A DLQ publication failure is not treated as successful parking; it causes delayed redelivery of the original message.

## Trust boundary

- Message tenant/caller fields are not authorization. A worker recovers trusted execution context from PostgreSQL.
- MessageHandlerError carries only classification, error code and diagnostic text; it does not create a tenant Scope or grant authority.
- Broker SDK types remain inside the qembed-api adapter.

## Validation

- Integration tests use a real NATS JetStream server and PostgreSQL 19.
- Verify delayed retry for a retryable failure.
- Verify permanent and exhausted failures are copied to the DLQ and the source is ACKed only after DLQ PubAck.
- Verify DLQ envelope metadata and raw payload preservation.
