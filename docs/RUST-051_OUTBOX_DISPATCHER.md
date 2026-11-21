# RUST-051a — PostgreSQL outbox claim primitive

## Scope

This change implements the PostgreSQL-side claim/lease primitive used by the future Outbox → NATS JetStream Relay. It is **not** the complete RUST-051 relay and does not publish to NATS.

## Implemented contract

- Restricted qembed_dispatcher role sees only the delivery projection; it cannot read events, event_id, aggregate identity or other domain tables.
- A short transaction claims pending rows with FOR UPDATE SKIP LOCKED, assigns owner/expiry, increments the monotonic claim fencing token and attempts counter, and returns only delivery fields.
- Network I/O must happen after the claim transaction commits.
- The future JetStream publisher must publish with stable delivery_id as Nats-Msg-Id, wait for PubAck, then persist a publication receipt in PostgreSQL.
- Acknowledgement/receipt updates are accepted only for the current owner and fencing token. A stale relay cannot finalize a claim reclaimed by a newer relay.

## Delivery semantics

- The end-to-end contract is at-least-once, not exactly-once.
- A crash after JetStream PubAck but before PostgreSQL publication receipt causes a safe duplicate publication.
- JetStream deduplication is bounded by its configured window. Consumers must remain idempotent using stable delivery_id and business identities such as invocation_id/command_id.
- PubAck is not worker acknowledgement and is not proof of business completion.
- PostgreSQL Outbox remains the recovery source; this PR alone does not implement the JetStream publisher, durable consumer, or recovery sweeper.

## Follow-up

- RUST-051b adds the broker-neutral publisher port and JetStream adapter.
- RUST-051c tests relay failure windows against a real JetStream server.
- RUST-052 adds durable pull consumers, ACK-after-commit, retry/backoff, parking/DLQ and PostgreSQL-driven recovery.

## Operational boundary

The application DB login must be granted membership in the NOLOGIN qembed_dispatcher role. The role remains restricted to the delivery projection.
