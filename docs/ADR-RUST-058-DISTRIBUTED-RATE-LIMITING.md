# RUST-058 — Distributed rate limiting

## Status

Accepted for implementation in RUST-058.

## Context

RUST-058 requires request limits to be consistent across multiple runtime workers and isolated for the `Anonymous`, `Authenticated`, and `Owner` request classes. The limiter state must not be stored in PostgreSQL.

The existing PostgreSQL token-budget reservation system is a separate concern. Its `InvocationPool` currently has `anonymous` and `authenticated` values and remains authoritative for durable token usage. RUST-058 must not alter that accounting or reinterpret `Owner` as a third token-budget pool.

## Decision

- Use Valkey as the first distributed rate-limit backend.
- Access Valkey through the Redis RESP protocol using the Rust `redis-rs` client.
- Use a Token Bucket per rate-limit key.
- Execute refill, decision, decrement, and TTL update in one server-side Lua script.
- Read time from Valkey (`TIME`) inside the script; runtime worker clocks do not determine refill.
- Keep policy and request-class selection in Rust. Valkey stores only ephemeral bucket state.
- Keep the storage boundary behind a provider-neutral `RateLimitStore` port and a runtime `RateLimitService`.
- Keep `Anonymous`, `Authenticated`, and `Owner` request buckets isolated. Consumption by one class must not consume another class's reserved capacity.
- Do not add a shared aggregate bucket in this change.
- Do not add PostgreSQL tables, migrations, or SQLx queries for rate limiting.
- Deployment topology is not fixed by this ADR. The adapter must not assume a single runtime process or a specific Valkey topology.

## Failure semantics

The selected guarantee is practical distributed rate limiting, not strict quota enforcement. Normal concurrent requests against the same Valkey state are serialized by the atomic script. Valkey failover may lose recently acknowledged counter updates because replication is asynchronous; a small temporary overrun is accepted.

If Valkey is unavailable, the initial API integration uses fail-open for ordinary invocation admission, logs an observable error, and does not silently substitute per-process counters. Sensitive endpoints may adopt fail-closed behavior through an explicit policy in a later change.

## Key scope

A bucket key must include the tenant, object, request class, and a server-established subject identifier where the class requires per-subject isolation. Keys used together in a future multi-bucket atomic operation must share a Valkey Cluster hash slot. No client-supplied class or tenant value is trusted as authentication evidence.

## Policy configuration

Numerical capacities and refill rates are configuration, not hard-coded product policy. RUST-058 defines validated policy types and requires explicit policy injection at composition. Production values and deployment topology remain deployment configuration.

## Validation

- Unit tests cover policy validation, token refill, burst capacity, retry-after calculation, and class isolation.
- Multi-worker integration tests use independent Valkey connections and concurrent consumers.
- Tests verify that the three request classes do not consume each other's buckets.
- Multi-worker tests use independent connection managers; Valkey's reconnecting connection manager is used for transient connection loss. Actual replication failover behavior remains a deployment-level acceptance test: the accepted bounded-overrun limitation is documented, not misrepresented as a strict guarantee.
- Existing PostgreSQL budget reservation tests remain unchanged and continue to run.
