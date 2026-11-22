# RT-050 — KMCP adapter ports and trust boundaries

**Status:** Contract implementation in progress  
**Code:** crates/qembed-kmcp/src/ports.rs  
**Parent:** PR #96 — lifecycle policy core  
**Deployment:** embedded administrative control plane first; dedicated service remains an extraction option  
**Provider:** provider-neutral; Selectel or customer-operated infrastructure may supply adapters after capability review

## 1. Decision: embedded first

The first deployment does not require a separately operated KMCP network service. Policy and orchestration are a Rust crate used by a restricted administrative CLI or later administrative API process. PostgreSQL is authoritative for durable registry state, operation idempotency, audit history and outbox delivery state.

This does not permit KMCP to enter the runtime request path. Runtime identities cannot invoke administrative operations. A dedicated service may be extracted later if operational isolation, multiple deployments or audit requirements justify it.

## 2. Trust model

### 2.1 Identity provenance

The administrative transport accepts opaque credentials only. It must not accept actor_subject, approver_subject, role names or permission arrays as trusted request fields. AdminAuthorizer validates the credential and requested deployment/operation, then returns an AuthorizedAdmin capability.

AuthorizedAdmin::from_verified_claims is a trust boundary, not a JWT validator. Only trusted identity-adapter code may call it after validating issuer, signature, audience, expiry, MFA/authentication context and deployment-scoped authorization. Activation and planned retirement require independent authorization of operator and approver; the policy core checks that their stable subjects differ.

Emergency revocation requires break-glass permission and an incident reference. A second string in the same request is not proof of a second human.

### 2.2 Key custody

KeyProvider provisions Ed25519 keys and returns only immutable key ID, 32-byte public key, opaque provider reference and signature over a short-lived proof-of-possession challenge. Private bytes never enter ProvisionedKey or PostgreSQL.

This administrative port is not the per-token signer. Context issuance continues through the existing synchronous ContextSigner contract, whose implementation must be bounded and local. No remote KMS call may occur while the issuer holds a PostgreSQL transaction or delegation row lock.

### 2.3 Registry authority

KeyRegistry is the only KMCP port for durable lifecycle state. A production PostgreSQL adapter calls narrowly scoped SECURITY DEFINER functions and grants no direct table DML to the KMCP identity.

One registry operation atomically commits: (1) lifecycle state and incremented version; (2) request-ID idempotency record bound to canonical operation and authenticated subjects; (3) append-only audit event; and (4) durable outbox row.

expected_version is the compare-and-set condition. A stale version returns CompareAndSetConflict; orchestration reloads and recomputes policy, never overwriting newer state.

### 2.4 Audit delivery

AuditOutboxExporter claims durable events with a lease. Delivery is at-least-once; the sink deduplicates by immutable event ID. Acknowledge only after durable sink acceptance. Failure leaves the event retryable and does not roll back lifecycle state.

The exporter has a distinct database identity from control-plane and runtime identities, with only claim, sanitized-failure and acknowledgement function privileges.

## 3. Port ownership

| Port | Owns | Must not own |
|---|---|---|
| AdminAuthorizer | Authenticate operator, deployment scope and operation permission | Trust client-supplied actor, approver or role fields |
| KeyProvider | Generate Ed25519 key, sign possession challenge, disable/destroy | Persist lifecycle state or choose deployment |
| KeyRegistry | CAS, durable idempotency, state, audit and outbox transaction | Broad direct table DML |
| AuditOutboxExporter | Lease, deliver, retry and acknowledge immutable events | Change key lifecycle or bypass outbox |
| ContextSigner | Bounded local signing on request path | Remote network KMS operations |

All ports are synchronous in this slice. Implementations must document bounded latency and blocking behavior. Network operations belong outside PostgreSQL transactions.

## 4. Provisioning and orphan-key reconciliation

Provider and PostgreSQL do not share a distributed transaction. Required sequence:

1. Authorize StageKey for the target deployment.
2. Generate a unique key ID and cryptographically random short-lived proof challenge.
3. Provision Ed25519 material and sign the challenge through KeyProvider.
4. Verify proof against the returned public key before registration.
5. Persist public key and provider reference as staged with request-ID idempotency.
6. If persistence fails after provider creation, preserve reconciliation evidence in a protected operational channel; retry registration idempotently or disable/destroy the orphan under approved policy.
7. Reconcile provider inventory against registry; never silently import or delete unknown keys.

The first adapter must make this failure window observable. It must not claim atomicity across provider and PostgreSQL.

## 5. Failure semantics

| Failure | Required behavior |
|---|---|
| Authentication/permission failure | Stop before provider or registry side effects |
| Provider provisioning failure | No registry change; return typed provider error |
| Proof-of-possession failure | Do not register; quarantine/disable and emit sanitized security evidence |
| Registry CAS conflict | Reload and recompute; never retry stale decision |
| Same request ID and payload | Return original result without another transition |
| Same request ID, different payload | Reject as idempotency conflict |
| DB failure after provider creation | Preserve reconciliation evidence and provider reference |
| Audit sink unavailable | Keep outbox pending; do not undo lifecycle state |
| Export acknowledgement failure | Retry same event ID; sink deduplicates |
| Provider disable/destroy failure | Keep registry conservative and raise incident; never reactivate revoked key |

## 6. Data handling

- Adapter errors are typed and sanitized; never include credentials, private bytes or token payloads.
- Provider references are sensitive operational metadata.
- Audit includes subjects, reason, incident reference, deployment/key IDs, transition and timestamps; excludes private material and credentials.
- At-least-once delivery is not exactly-once; exactly-once effects require sink-side event-ID deduplication.
- Clock authority and skew monitoring remain explicit.

## 7. Non-goals and open gates

This contract does not implement OIDC, a production HSM, PostgreSQL adapter, external audit sink, HTTP service, deployment manifests or operational reconciler. The verified-claims constructor does not validate JWTs. Interfaces are not evidence that adapters are secure.

QSA-003 remains open. Production adoption still requires canonical SQL migration review, independent security review, multi-deployment replay/revocation tests, signer outage tests, C-extension fuzzing, packaging/provenance and supported PostgreSQL version coverage.

## 8. Next verification criteria

- Contract tests for identity provenance and deployment scoping.
- Fake provider tests for provisioning, proof failure, disable and destroy.
- Registry tests for CAS conflict and idempotency payload binding.
- Outbox tests for lease expiry, retry and duplicate sink delivery.
- Fault injection across provider creation and registry persistence.

## 9. Application orchestration slice

The orchestration entry points now live in `crates/qembed-kmcp/src/orchestration.rs`:

- `stage_key`: authorize the operator, replay-check the durable request ID, provision through the provider using that request ID as its idempotency key, verify the Ed25519 proof-of-possession challenge, then atomically persist the staged registry row, operation idempotency, audit event and outbox.
- `activate_key` and `retire_key`: authorize actor and approver independently, enforce deployment scope and explicit permission for both, then compute policy and persist with bounded compare-and-set retries.
- `revoke_key`: require the emergency-revoke permission and let the pure lifecycle policy enforce an incident reference and immediate revocation.
- Every mutation checks the stored idempotency result before recomputing policy. A retry with the same request ID but different actor, approver, reason, incident reference, action or retirement grace is rejected.

### Provider/registry partial failure

The provider provisioning port now accepts `request_id` as a provider-side idempotency key. The provider contract requires the same deployment/request/key tuple to resolve to the same key identity on retries while signing the newly supplied challenge. This closes duplicate-key creation when provider creation succeeds but PostgreSQL registration fails.

The proof challenge is generated from the operating system CSPRNG. The service verifies the Ed25519 signature against the returned public key before registering it. If proof verification fails, provider disable is attempted best-effort. This is not a substitute for a durable provider-inventory reconciler: provider/registry distributed atomicity does not exist, and the adapter must expose unknown or unregistered keys for reconciliation.

### Concurrency and replay

The registry must check request-ID idempotency before the CAS predicate. A stale version is never overwritten. The service reloads and recomputes up to three times after CAS conflicts; if a concurrent identical request committed, it returns the original stored result. Reuse of an idempotency key with a different payload is a hard error.

### Explicit limitations

This slice supplies orchestration logic and adapter contracts only. It does not yet include a production OIDC implementation, concrete PostgreSQL `KeyRegistry`, production key provider, durable orphan-key reconciler, audit sink worker, CLI/HTTP transport, or signer/key disable coordination. The `AuthorizedAdmin::from_verified_claims` constructor remains a trust-boundary API: only the injected trusted authorizer may call it after real credential verification. This is not a claim that Rust's type system authenticates arbitrary third-party trait implementations.

QSA-003 remains open until the signed context replaces all legacy identity-GUC authorization reads in the canonical SQL schema, all SQL/Rust integration tests pass against the target PostgreSQL release, and the production deployment/security gates are satisfied.
