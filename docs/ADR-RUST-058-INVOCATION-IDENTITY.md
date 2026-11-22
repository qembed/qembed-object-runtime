# RUST-058 — Invocation API identity model

## Status

Accepted for the expanded RUST-058 implementation.

## Decision

The HTTP API exposes three invocation admission classes. All three use the same Runtime invocation-start use case and the same provider-neutral rate-limit port, while retaining separate Valkey Token Bucket keys and policies.

| Route | Authentication context | Caller authorization | Rate-limit class |
|---|---|---|---|
| `POST /anonymous/objects/{object_id}/invocations` | No client credential | Public invocation policy for the resolved object; no client-selected Caller | `Anonymous` |
| `POST /authenticated/objects/{object_id}/invocations` | Qembed-issued Principal API token | Client selects a Caller; PostgreSQL verifies an active direct Principal-to-Caller delegation | `Authenticated` |
| `POST /owner/objects/{object_id}/invocations` | Existing TenantOwner credential | Owner selects a Caller; existing PostgreSQL delegation check remains authoritative | `Owner` |

External JWT/OIDC is explicitly out of scope for this change. The authentication boundary remains replaceable so a future JWT/OIDC adapter can establish the same trusted Principal identity without changing the Runtime invocation use case.

## Principal token semantics

An Authenticated API token authenticates a Principal; it is not a Caller token and does not itself grant permission to invoke as an arbitrary Caller.

- The token is high entropy and Qembed-issued.
- Only a cryptographic digest is persisted.
- The verifier resolves the token to a Principal and tenant before creating a trusted `Scope`.
- The request supplies the selected `caller_id` separately.
- The Store verifies the active, direct delegation for that exact Principal, tenant, and Caller in the invocation transaction.
- Unknown, revoked, disabled, and tenant-mismatched credentials produce an indistinguishable authentication failure.
- Request DTOs never set `app.tenant_id` or `app.principal_id`; only trusted authentication/composition code may establish those transaction-local settings.

## Anonymous invocation semantics

Anonymous invocation does not accept a bearer credential and does not accept a client-selected Caller ID. The server establishes the anonymous execution identity after resolving the target object's tenant and checking its explicit public-invocation eligibility.

The object must be active, have a published version, and be explicitly enabled for public invocation. An `active` lifecycle state alone is not proof of public visibility. The implementation must fail closed when the public-invocation policy is absent or disabled.

The anonymous identity and Caller must be server-controlled and tenant-scoped. The implementation must not use a caller-supplied UUID, a forged `Scope`, or an implicit self-delegation as a substitute for the trusted public-invocation path.

## Rate-limit identity

- Owner buckets are keyed by authenticated Principal.
- Authenticated buckets are keyed by the authenticated Principal, not by the selected Caller. A client therefore cannot evade its request quota by rotating among delegated Callers.
- Anonymous buckets use a server-derived anonymous subject. The derivation must be documented, bounded, and safe behind configured trusted proxies; raw IP addresses must not be stored in Valkey keys or logs.
- Class separation is logical quota separation. It does not claim physical connection-pool isolation or weighted fairness between classes.
- PostgreSQL token budgets and `InvocationPool` remain independent from request-rate limiting.

## Idempotency and admission order

Authentication and authorization must happen before rate-limit admission. An idempotent replay must not consume another rate-limit token. The implementation must preserve the existing atomic idempotency claim and must not move provider/network I/O into a database transaction. If replay detection cannot be performed safely before admission without introducing a race or weakening authorization, the Store/runtime contract must expose an explicit replay-aware admission step rather than relying on an unprotected preflight query.

## Required persistence changes

The current delegation schema ties `principal_caller_delegations.principal_id` to `owner_principals`. That is insufficient for non-owner authenticated Principals. The implementation must generalize the Principal persistence boundary and delegation foreign keys without weakening tenant scoping, row locking, append-only delegation history, or revocation serialization.

Anonymous public invocation must use a separate trusted Store path or equivalent database capability. It must not manufacture a regular authenticated Principal from unauthenticated request data.

## Operational behavior

Valkey remains ephemeral rate-limit state. The current accepted fail-open behavior on Valkey outage remains explicit and observable through structured logging and metrics; no per-process fallback limiter is introduced.

The initial Valkey adapter continues to use one configured RESP endpoint. Native Sentinel/Cluster routing remains deferred until deployment topology is selected.
