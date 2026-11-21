# RUST-051 — Lease-fenced outbox dispatcher

## Contract

The Store adapter claims outbox deliveries across tenants using the dedicated
`qembed_dispatcher` PostgreSQL role. The role receives column-level access to
the delivery projection only; it cannot read `events`, `event_id`, aggregate
identity, or other domain tables.

A claim is one short transaction:

1. Select pending rows for a subscribed topic, ordered by creation time.
2. Lock candidates with `FOR UPDATE SKIP LOCKED`.
3. Assign an owner and expiry, increment the claim fencing token and attempt
   counter, and return only tenant ID, delivery ID, topic, payload, attempts,
   and claim token.
4. Commit before the caller performs delivery I/O.

Delivery I/O must happen outside the database transaction. A successful
consumer call is followed by `acknowledge`; a failed call may `release` the
claim for immediate redelivery. If a process crashes, the lease expires and a
different dispatcher can reclaim the row.

Acknowledgement is accepted only for the current owner, current fencing token,
and unexpired lease. A stale worker cannot acknowledge a row reclaimed by a
newer worker. The stable `delivery_id` is the consumer idempotency key: a
consumer must persist/deduplicate that identity before applying its side
effect. The integration test exercises this deduplication rule across a
release/reclaim cycle.

## Operational boundaries

- Delivery is at-least-once, not exactly-once. A crash after consumer success
  but before acknowledgement intentionally causes redelivery.
- Consumers must be idempotent by `delivery_id`; the dispatcher cannot make an
  external side effect and PostgreSQL acknowledgement atomic.
- Retry scheduling, maximum attempts, poison-message policy and dead-letter
  handling belong to RUST-052.
- This change provides the claim/delivery/ack persistence primitive. It does
  not add a broker-specific transport or a long-running process supervisor.
- The application database login used by this adapter must be granted
  membership in the NOLOGIN `qembed_dispatcher` role. The role itself remains
  restricted to the delivery projection.
