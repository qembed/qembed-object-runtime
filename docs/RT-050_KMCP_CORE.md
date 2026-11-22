# RT-050 — Provider-neutral KMCP core contract

**Status:** Implementation slice in progress  
**Related ADR:** [ADR-RT-050-KMCP](ADR-RT-050-KEY-MANAGEMENT-CONTROL-PLANE.md)  
**Code:** `crates/qembed-kmcp`  
**Deployment target:** Selectel or customer-operated infrastructure; no provider selected

## 1. Purpose and scope

The KMCP is an administrative control plane for verification/signing-key lifecycle. It is not part of the request path and must not be called for each context token.

This slice establishes a provider-neutral, deterministic lifecycle policy module. It is deliberately smaller than a deployable KMCP service: it validates legal state transitions and the metadata required by each operation. It does not yet authenticate operators, provision keys, persist operations, expose HTTP endpoints, or deliver external audit events.

The separation is intentional. The same policy code must be usable whether the deployment later uses Selectel, an on-premises HSM, a local signer agent, or another approved provider.

## 2. Trust boundaries

| Boundary | Responsibility | Explicitly not trusted to do |
|---|---|---|
| HTTP/API adapter (future) | Parse request, enforce transport limits, attach authenticated session | Treat actor/approver strings from JSON as authenticated identity |
| OIDC/SSO authorizer (future) | Validate issuer, signature, audience, expiry, MFA/authentication context, groups and deployment-scoped permissions | Delegate authorization to caller-supplied role names |
| KMCP policy core (this slice) | Validate state transition, separation of duties, required incident reference and retirement bounds | Authenticate identities or persist state |
| Key provider (future) | Generate Ed25519 key, prove possession, return public key and opaque provider reference; disable/destroy key | Choose tenant identity or mutate PostgreSQL registry |
| PostgreSQL registry adapter (future) | Execute approved SECURITY DEFINER functions with compare-and-set semantics | Give KMCP direct table DML or runtime registry access |
| Audit exporter (future) | Deliver immutable event to external sink, acknowledge by event ID | Mutate lifecycle state or read tables directly |
| Runtime signer | Sign exact JCS bytes using bounded local capability | Stage/activate/retire/revoke keys |

The actor and approver fields accepted by `LifecycleRequest` are authenticated facts supplied by the trusted service composition layer, not proof by themselves. The production service must never construct them from an untrusted request DTO. The injected authorization adapter remains a critical trusted component and needs its own tests and review.

## 3. State machine

Production lifecycle:

`absent -> staged -> active -> retired -> revoked`

The legal transition graph is intentionally not a simple linear chain: a staged or active key may be emergency-revoked, and a retired key may be emergency-revoked. No transition leaves `revoked`; no reactivation is supported; key IDs are never reused within a deployment.

| Action | Required current state | Required evidence | Result |
|---|---|---|---|
| Stage | Absent | Authenticated operator, request ID, reason; provider proof-of-possession must be verified by the future provisioning workflow | Staged; cannot sign or verify |
| Activate | Staged | Authenticated operator and distinct authenticated approver | Active |
| Retire | Active | Authenticated operator and distinct authenticated approver; grace 1–3600 seconds | Retired; no new signing; bounded verification |
| Revoke | Staged, Active or Retired | Authorized break-glass operator and non-empty incident reference | Revoked immediately; no grace |

The initial protocol policy uses 120 seconds as the default retirement grace and 3600 seconds as the hard maximum. This module validates the allowed range but does not silently choose the default; the caller must pass an explicit value after applying deployment policy.

For retirement, `verify_until = now + grace` is computed with checked arithmetic. Overflow is rejected. The verifier must treat the deadline as exclusive: current database time must be strictly earlier than `verify_until`, and token expiry must not exceed it. These verifier checks remain SQL responsibilities and are not implemented by this policy function.

## 4. Idempotency and concurrency contract

Every administrative mutation carries a stable request ID. This crate validates that it is present but does not implement idempotency storage.

The PostgreSQL adapter must:
1. Bind the request ID to the complete canonical operation payload and authenticated actor/approver metadata.
2. Return the original operation result for an identical retry.
3. Reject reuse of the same request ID with a different payload.
4. Read the current key state and apply the planned transition with a row-lock/CAS condition in one transaction.
5. Commit the key state change, append-only audit event, and audit-outbox row atomically.
6. Return a conflict when another operation has changed the state since the caller read it; never overwrite a newer state with a stale decision.

A policy decision is not a reservation and does not guarantee that persistence will succeed. The adapter must recompute after a conflict.

## 5. Time and clock ownership

The caller supplies the operation timestamp from a trusted KMCP clock. The core performs checked integer arithmetic only; it does not read the system clock.

The PostgreSQL verifier remains authoritative for token acceptance time and uses database time. KMCP and PostgreSQL clock synchronization, skew monitoring, and alerting are operational requirements. A future service implementation must document the clock source and skew budget.

## 6. Error behavior

Policy errors are deterministic and contain no secret values. They distinguish missing evidence, invalid transitions, key-ID reuse, and out-of-range retirement grace. The HTTP adapter should map them to stable machine-readable error codes without leaking provider references, tokens, or credentials.

An authorization failure must be rejected before this policy function is called. Provider, database, and audit-sink failures are outside this module and must be represented by separate typed adapter errors in the next implementation slice.

## 7. Provider-neutral deployment contract

No provider-specific SDK or API is allowed in `qembed-kmcp`.

The future provider adapter must expose the minimum Ed25519 capabilities needed by RT-050:
- Generate a new key in the deployment's configured custody boundary.
- Return a 32-byte public key and opaque provider key reference.
- Sign a short-lived proof-of-possession challenge.
- Sign exact canonical JCS bytes through a bounded local capability for request-path use.
- Disable/revoke and, where policy permits, destroy key material.
- Report algorithm, key status and provider reference without exposing private bytes.

A cloud secret store is not automatically a signing-capable KMS/HSM. Selectel Secret Manager, for example, must not be treated as an Ed25519 signer until its supported algorithms, non-exportability, signing API, availability, latency and recovery behavior have been verified. If the selected service cannot provide the bounded local signing path required by RT-050, it may store wrapped material but cannot be used as the per-token remote signer.

## 8. Explicit non-goals of this slice

- OIDC discovery, JWT validation, MFA or RBAC implementation.
- HTTP server, routes, admin UI or deployment manifests.
- Key generation, proof-of-possession cryptography or KMS/HSM integration.
- PostgreSQL SQL functions, migrations or direct registry access.
- Durable idempotency, outbox polling, retries or external immutable audit sink.
- Multi-deployment revocation propagation.
- Production approval of SQL PR #69 or closure of QSA-003.

These are not implicitly satisfied by the presence of lifecycle policy code.

## 9. Verification plan

Unit tests cover:
- absent-to-staged registration;
- staged-to-active with two distinct subjects;
- active-to-retired with explicit bounded grace;
- retirement deadline overflow;
- immediate revocation from retired state;
- missing incident reference;
- invalid, reverse and repeated transitions;
- unknown key IDs;
- unexpected operation metadata.

CI must additionally run workspace formatting, all-target check, Clippy with warnings denied, crate-boundary validation, PostgreSQL integration suite and dependency checks. The pure policy tests do not prove the database transaction, authentication, provider custody, or audit delivery guarantees.

## 10. Next implementation slices

1. **Authorization contract:** deployment-scoped permissions, authenticated actor/approver provenance, dual-control and break-glass policy.
2. **Provider and registry ports:** typed contracts for key provisioning/proof-of-possession, local signing, SQL lifecycle functions, and audit-outbox delivery.
3. **Durable operation orchestration:** idempotency, compare-and-set, failure/retry states, orphan-key reconciliation.
4. **HTTP service adapter:** administrative endpoints behind OIDC/SSO; no runtime API coupling.
5. **Deployment adapters:** selected OIDC provider, key custody/HSM/local signer and immutable audit sink after capability verification.
6. **Adversarial integration tests:** concurrent operations, partial provider/database failures, signer outage, audit backlog and multi-deployment revoke.

Until these slices and the existing SQL/security gates are complete, KMCP remains an implementation prototype, not a production security boundary.
