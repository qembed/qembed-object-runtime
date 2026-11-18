# Invocation leases and fencing (RUST-038)

## Purpose

RUST-037 persists the invocation lifecycle but does not assign exclusive ownership to a worker. RUST-038 adds PostgreSQL-backed leases so workers can compete safely and stale workers cannot commit state or output after a reclaim.

This contract uses the existing canonical columns on `object_invocations`:

- `lease_owner TEXT`
- `lease_until TIMESTAMPTZ`
- `heartbeat_at TIMESTAMPTZ`
- `fencing_token BIGINT`
- `version BIGINT`

No schema change is part of this task.

## Ownership and fencing are both required

Every worker write must be conditional on all of:

1. trusted tenant scope and invocation identity;
2. the worker's exact `lease_owner`;
3. `lease_until > now()`;
4. the expected `fencing_token`;
5. the expected optimistic `version` and expected lifecycle state.

The owner predicate is required even before the first reclaim. The canonical trigger permits the initial lease assignment without changing the token. Therefore, token-only CAS would allow a process that read the row before the first claim to write with the initial token.

The fencing token protects the reclaim boundary: once an expired lease is reclaimed, the token increases monotonically and every write from the former owner is rejected. The owner predicate protects the initial claim and prevents a different worker from using the same token.

## Claim, heartbeat, and reclaim

All operations are single PostgreSQL statements (or a transaction where coupled writes are needed), use database time, and are tenant-scoped.

### Initial claim

A worker may claim an invocation only when both lease columns are NULL and the invocation is nonterminal. It sets owner, expiry, and heartbeat timestamp. It does not change `version` or `fencing_token`, matching the canonical database trigger.

### Heartbeat

The current owner may extend its lease only while `lease_until > now()` and only when the supplied fencing token still matches. It updates `lease_until` and `heartbeat_at`, but not lifecycle version or fencing token.

An expired lease cannot be revived by heartbeat. It must be reclaimed, even if the same process attempts to renew it.

### Reclaim

A worker may reclaim only when the existing lease is expired (`lease_until < now()`) and the invocation is nonterminal. Reclaim atomically assigns the new owner, extends the lease, updates heartbeat, and increments both `fencing_token` and `version` exactly once.

Initial claim and reclaim must be distinguishable in SQL. Reclaim must never be implemented as an owner-only update of an expired lease: the canonical trigger requires a strictly newer token and a version increment.

## Lifecycle writes and terminal release

Every lifecycle CAS, assistant-output insert, failure write, and completion write includes owner, unexpired lease, token, version, and state predicates. The assistant message insert and Processing → Verifying transition remain one transaction.

Terminal completion/failure clears `lease_owner` and `lease_until` in the transaction that records the terminal state. The canonical SQL trigger rejects changing the owner of a still-live lease, but explicitly permits release on a terminal transition after expiry. The adapter therefore first moves `lease_until` into the past and then records the terminal state/releases the owner in a second update under the same transaction and row lock; no competing worker can observe the intermediate state. Heartbeat metadata may remain as historical data.

A worker that loses ownership or whose lease expires receives a conflict result. It must stop processing that invocation; it must not silently reload and continue under an obsolete claim.

## External provider calls

Provider I/O remains outside SQL transactions. The worker heartbeats while waiting for a provider response. If the lease is lost, the worker may be unable to cancel the provider request, but it cannot persist the response or a terminal state.

This does not provide exactly-once provider execution. A process can still die after the provider completes and before the response is durably committed. A later owner may call the provider again. Avoiding that duplicate requires provider-supported idempotency or a separate durable execution/reconciliation contract.

## Required PostgreSQL concurrency tests

Use at least two independent database sessions:

1. Worker A claims an unleased invocation; worker B cannot claim it.
2. Worker A heartbeats before expiry; the lease deadline moves forward.
3. An expired lease cannot be heartbeated by its former owner.
4. Worker B reclaims an expired lease; token and version each advance exactly once.
5. After reclaim, worker A cannot transition state, persist assistant output, fail, or complete—even if it retained its old in-memory Invocation.
6. Worker B can perform the same valid transition with the new token.
7. A terminal invocation cannot be claimed or reclaimed.
8. A tenant-scoped operation cannot claim or mutate another tenant's invocation.

These tests prove the database boundary, not merely an in-memory worker convention.
