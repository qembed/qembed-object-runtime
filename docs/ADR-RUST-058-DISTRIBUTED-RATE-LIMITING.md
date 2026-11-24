# RUST-058 — Distributed request rate limiting

## Status

Accepted implementation architecture. Implementation is delivered in staged pull requests; this document does not claim the feature is complete until all stages are merged.

## Context

Request limits must be consistent across runtime workers and isolated for the `Anonymous`, `Authenticated`, and `Owner` invocation classes. Rate-limit state is ephemeral and must not be stored in PostgreSQL.

PostgreSQL `InvocationPool` and token-budget reservations remain the authoritative durable accounting system. Request throttling is a separate concern and must not alter those semantics or add an Owner token-budget pool.

## Decision

- Use Valkey as the first distributed rate-limit backend over Redis RESP via `redis-rs`.
- Use a Token Bucket per rate-limit key.
- Refill, decide, consume, and refresh TTL in one atomic server-side Lua script, using Valkey `TIME`.
- Keep policy and request-class selection in Rust, behind a provider-neutral `RateLimitStore` port and runtime `RateLimitService`.
- Place the Valkey infrastructure adapter in `qembed-store`; `qembed-runtime` remains backend-agnostic and depends only on the domain port.
- Preserve fractional refill progress as an integer remainder and reject adapter policies whose refill products exceed Lua's exact IEEE-754 integer range.
- Keep Anonymous, Authenticated, and Owner buckets isolated. No aggregate bucket is introduced in this change.
- Add no PostgreSQL tables, migrations, or SQLx queries for rate limiting.
- Initial adapter topology is one configured RESP endpoint using redis-rs `ConnectionManager`; native Sentinel/Cluster routing is deferred until deployment topology is selected.
- Keys include a tenant/object hash tag for future Redis Cluster slot compatibility.

## Identity and key provenance

- Owner bucket identity is the authenticated TenantOwner Principal.
- Authenticated bucket identity is the authenticated Principal, never the selected Caller. Rotating among delegated Callers must not evade a Principal's quota.
- Anonymous bucket identity is an HMAC-SHA256 fingerprint derived from the direct peer IP with a server-held key and scoped to tenant + Object. The HTTP handler does not trust `X-Forwarded-For`; behind a reverse proxy, the immediate proxy peer is used unless a separate trusted-proxy policy is explicitly designed. Raw IP addresses and raw fingerprints are not written to Valkey keys or logs.
- Request class, tenant, Principal, Caller, and public-object eligibility are established by trusted server/authentication and Store paths; client DTO fields are not authentication evidence.
- The three classes have separate capacities/policies. This is logical quota isolation, not a claim of physical connection-pool isolation or weighted fairness.

## Failure semantics

The guarantee is practical distributed rate limiting, not strict quota enforcement. Normal concurrent operations against the same Valkey state are serialized by the atomic script. Asynchronous replication failover may lose recently acknowledged updates; bounded temporary overrun is accepted.

Valkey outage uses fail-open for ordinary invocation admission, with structured observable logging/metrics. No per-process fallback counter is allowed. Any fail-closed exception requires an explicit policy decision.

## Idempotency ordering

Authentication and Caller authorization precede rate admission. The implementation uses two coordinated safeguards without holding a PostgreSQL transaction open during Valkey I/O:

1. A short PostgreSQL preflight transaction resolves/locks the requested Caller and checks for an already-committed idempotency record. A known replay skips rate admission; a payload mismatch is rejected before touching Valkey.
2. For a new key, one Valkey Lua script atomically checks a request marker and mutates the Token Bucket. For Owner/Authenticated, the marker identity is a digest of tenant, Object, effective Caller and idempotency key. For Anonymous, both the Valkey marker and PostgreSQL idempotency key additionally include the server-derived fingerprint digest: all Anonymous subjects share the Object's stable Caller for durable budget attribution, but cannot collide with one another's idempotency keys. Marker identity is independent of rate class, and its TTL covers the PostgreSQL idempotency retention window.

The normal PostgreSQL begin-invocation transaction still re-resolves authorization and performs the authoritative idempotency reservation. Concurrent first attempts may both pass the preflight, but the shared Valkey marker charges the key once and PostgreSQL still creates at most one invocation. The marker is not an idempotency authority: Valkey failover may lose it and cause a bounded extra rate charge, but cannot create duplicate durable invocations. Provider/network I/O remains outside PostgreSQL transactions.

## Staged implementation and validation

1. Domain/runtime contracts and trusted rate-limit key construction.
2. Valkey adapter in `qembed-store`, atomic Lua Token Bucket, reconnecting connection manager, and independent-connection concurrency tests.
3. SQL-backed Principal API-token/delegation admission and replay-aware HTTP route.
4. Signed qctx2 Anonymous context, Object-scoped Anonymous Caller provisioning/resolution, HMAC fingerprint, tenant-qualified public HTTP route, and fingerprint-isolated idempotency/rate admission.
5. Structured observability and full HTTP/PostgreSQL/Valkey acceptance coverage for all three classes.

The first stage does not claim that invocation routes are rate-limited. Each later stage must pass exact-head format, Clippy, workspace, PostgreSQL 19, and multi-worker Valkey tests before merge.

## Public invocation access policy

- The canonical Object setting is `anonymous_invocation_enabled BOOLEAN NOT NULL DEFAULT TRUE`.
- The public route includes both tenant ID and Object ID because Object IDs are tenant-scoped, not globally unique. Tenant ID is routing context; it does not create an authenticated Principal.
- The host must serve the Axum router with `into_make_service_with_connect_info::<SocketAddr>()`; if direct peer context is absent, the route fails closed. `AnonymousFingerprintKey` requires a secret of at least 32 bytes supplied from deployment secret management, never source control. Rotating this key intentionally creates a new fingerprint/rate/idempotency namespace.
- qctx2 is a separate exact-claim signed-context version. Anonymous claims require `principal_id: null`, `subject_kind: "anonymous"`, Object ID and a 32-byte server fingerprint. The bootstrap token has `caller_id: null`; PostgreSQL provisions/reuses the Object's stable Anonymous Caller, then the Store issues a second token containing that resolved Caller ID.
- The Anonymous Caller mapping is Object-scoped and does not persist per-request fingerprints. PostgreSQL locks the Object and Caller lifecycle rows through the invocation transaction. The Anonymous transaction identity is not represented by a synthetic Principal.
- A newly created Object, and an existing Object when this column is introduced, allows Anonymous invocation by default.
- Setting the flag to `false` makes invocation private: an authenticated Principal is required; Anonymous requests are rejected before rate admission.
- Tariffs, subscriptions, Billing integration, and plan-based entitlement checks are explicitly out of scope for RUST-058d. The current system has no authoritative Billing/entitlement source, and this stage must not invent one or accept client-supplied plan labels.
- The setting and access-policy boundary should remain separable so a future change can add an authoritative entitlement check to the operation that disables Anonymous invocation, without changing the invocation-time meaning of the flag or the public-by-default default.
- Until that future policy is designed, this implementation does not enforce a Corporate-tier restriction on changing the flag. Authorization to change Object settings continues to follow the existing trusted Object-owner authorization model.
- The default is intentionally public-by-default, not opt-in. Documentation and API names must describe that behavior accurately.
