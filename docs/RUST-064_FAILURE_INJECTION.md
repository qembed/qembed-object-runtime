# RUST-064 — M1b failure-injection matrix

## Contract

PostgreSQL is the source of truth for work and business state. JetStream is a durable transport, but a PubAck is not proof that a consumer completed the work. Delivery is at-least-once: every recovery path preserves the original `delivery_id`, and consumer-side PostgreSQL idempotency prevents duplicate business effects.

`Unknown` provider outcomes are never automatically replayed. Recovery is permitted only when PostgreSQL proves the provider attempt is `NotStarted` (or another explicitly safe pre-send state).

## Failure matrix

| Failure point | Expected PostgreSQL state immediately after failure | Expected JetStream state | Recovery and executable evidence |
|---|---|---|---|
| NATS connection fails before relay claim | Outbox remains pending; no lease, no attempt increment, no publication receipt | No message | Reconnect, then claim and publish the unchanged row. `nats_connect_failure_before_claim_leaves_outbox_unclaimed` |
| Relay process exits after claim, before publish | Claim lease and fencing token are committed; `delivered_at` remains NULL | No message | Lease expires; another relay claims with a newer fencing token and publishes the same ID. `crash_after_claim_before_publish_reclaims_same_delivery` |
| Server accepts publish, client loses PubAck | Outbox remains unacknowledged; lease is released for retry | One accepted message | Retry with the same `Nats-Msg-Id`; JetStream deduplicates within its configured window. `publish_accepted_but_puback_lost_retries_with_same_id_and_jetstream_deduplicates` |
| PubAck received, PostgreSQL receipt write fails | `delivered_at` remains NULL; no receipt history row is written | One accepted message | Expire/reclaim lease, republish same ID, then persist the new PubAck receipt. `puback_before_postgres_receipt_failure_replays_same_delivery_id` |
| PostgreSQL business commit succeeds, consumer ACK path fails | Business effect is committed and durable; message remains unacknowledged | Source delivery is eligible for redelivery | Restart the worker against the same durable consumer; repeated handler execution must be idempotent. `duplicate_jetstream_delivery_after_commit_has_one_postgres_effect` |
| Broker loses a message before consumer delivery | Outbox publication receipt exists; invocation remains stale/Received | Message and consumer state disappear with broker storage | PostgreSQL recovery sweeper marks the original Outbox row; relay republishes the original ID. Covered by `jetstream_storage_loss_before_and_after_delivery_replays_from_postgres` |
| Broker loses a message after consumer delivery but before business commit | Same durable Outbox state; no business completion is recorded | Previously delivered but unacknowledged message disappears with broker storage | Same PostgreSQL recovery path republishes the original ID; the replacement durable consumer receives it. Covered by the same broker-loss test |
| Worker process restart | PostgreSQL leases and Outbox rows survive process exit | Durable stream/consumer state survives an ordinary worker restart | New relay/consumer handles resume from PostgreSQL leases and durable consumer state. Covered by the child-process relay test, consumer-restart redelivery test, and NATS reconnect test |
| PostgreSQL recovery replay | Original Outbox payload and ID remain authoritative; replay marker is durable | Broker message may be absent or previously accepted | Sweeper marks only eligible stale Received work; relay clears the marker only after a new PubAck is persisted. Broker-loss test exercises sweeper → relay → new durable consumer end to end |
| Provider outcome is Unknown | Invocation/provider attempt remains Unknown; no replay marker is created | No new delivery is published | Require reconciliation; never auto-retry. `recovery_fails_closed_for_unknown_provider_outcome` |

## Test execution

- Normal workspace integration tests exercise the NATS connect failure, claim/publish, lost-PubAck, receipt-write, consumer ACK, and Unknown recovery cases against PostgreSQL 19 and real JetStream.
- `scripts/test-postgres.sh` runs the destructive NATS restart tests serially after the workspace suite. The broker-loss test removes and recreates only the NATS container and any anonymous volumes attached to it; PostgreSQL remains running and is not reset.
- The destructive test has a cleanup guard that recreates the NATS service if the test unwinds.
- The broker-loss test uses the canonical `invocation.received` topic required by the PostgreSQL recovery function and creates two stale Received invocations with provider attempts explicitly `NotStarted`: it delivers one message without ACK, leaves the other undelivered, removes the broker container, runs the real `PgInvocationRecoverySweeper`, republishes both original Outbox identities, and verifies two receipt-history entries per delivery and delivery to the replacement durable consumer.

## Safety boundary

This matrix proves recovery from the injected storage-loss scenario; it does not claim that JetStream alone provides zero-loss replication. Production durability still depends on the deployed stream's storage, replication, retention and backup policy. PostgreSQL remains the independent recovery source. No scenario permits automatic replay of an ambiguous provider side effect.
