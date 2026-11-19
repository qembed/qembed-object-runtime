# Token budget reservation (RUST-041b)

This document describes the token-only reservation performed while a new invocation is accepted. It complements [the begin-invocation transaction](BEGIN_INVOCATION_TRANSACTION.md) and the canonical physical contract in `qembed-sql`.

## Limits enforced at begin time

A new invocation must pass all of these checks:

1. **Per-invocation ceiling:** the effective ceiling is the lower of the immutable snapshot's `limits.max_tokens_per_invocation` and the current `object_budgets.max_tokens_per_invocation`. The live value can tighten the published snapshot limit, never raise it.
2. **Object daily token ceiling:** `daily_token_limit` applies to the sum of consumed and reserved tokens for the object and UTC usage date. It aggregates both invocation pools.
3. **Caller daily token ceiling:** `per_caller_daily_token_limit` applies to the sum of consumed and reserved tokens for this effective Caller and UTC usage date.
4. **Object concurrency ceiling:** active, unexpired `Reserved` rows for the object must be below `max_concurrent_invocations`.

A missing, malformed, fractional, zero, or negative snapshot cap fails closed. A missing/invalid live budget configuration also rejects the invocation; there is no unlimited fallback.

## Reservation estimate

RUST-041b reserves the mock-path input estimate before provider dispatch:

`ceil(input_utf8_bytes / 3) * 1.3`, rounded upward to a whole token.

The calculation uses UTF-8 byte length and checked integer arithmetic. A zero estimate is rejected by the reservation layer. The provider output allowance is separately bounded by the remaining effective per-invocation ceiling; provider usage normalization and settlement are subsequent RUST-041c work.

This stage is token-only. It writes `reserved_cost_micros = 0`; it does not enforce `daily_cost_limit_micros`, `max_llm_calls`, or `max_tool_calls`.

## Concurrency and transaction boundary

PostgreSQL is the authority for acceptance. Every reservation first locks the tenant/object row in `object_budgets` with `FOR UPDATE`. That row is the serialization point for object-wide daily usage and concurrency checks. Counter reads happen only after acquiring the lock; competing begin transactions for the same object therefore cannot both spend the same remaining capacity.

The reservation, object-pool counter increment, caller counter increment, invocation row, user interaction message, event, outbox row, and idempotency claim are part of one tenant-scoped PostgreSQL transaction. Any later failure rolls all of them back. The lock is released at commit or rollback.

The idempotency claim is checked before budget reservation. A matching replay returns the original invocation and does not create a second reservation or increment either counter. A payload mismatch remains an idempotency conflict.

Provider/network I/O is deliberately outside this transaction. It may begin only after the transaction commits and `begin_invocation` returns `Created`.

## Pool attribution and anonymous share

The pool (`anonymous` or `authenticated`) is retained in `budget_reservations` and `usage_counters` for attribution. The object daily token ceiling is shared across both pools. The runtime does not read or enforce `anonymous_share_pct`; it is intentionally excluded from this policy until a separate contract defines its semantics.

## Reservation lifecycle and future stages

At creation the reservation status is `Reserved`, its expiry is based on the configured `max_duration_ms`, and the estimated token amount is added to the reserved counters. Settlement, release, expiry reconciliation, and ambiguous provider outcomes are not implemented by RUST-041b; they are addressed by RUST-041c and RUST-041d. Until those stages land, do not interpret a reservation as final provider usage or release it merely because a caller timed out.

## Schema boundary

The canonical `qembed-sql` schema defines a deferred foreign key from `budget_reservations` to `object_invocations` and a caller foreign key from `caller_usage_counters` to `callers`. The current Rust store migration chain does not yet create those parent tables, so this migration documents but does not install those two constraints. They must be added when the corresponding parent-table migrations enter this chain. The reservation and its invocation are nevertheless written in the same transaction by the store implementation.
