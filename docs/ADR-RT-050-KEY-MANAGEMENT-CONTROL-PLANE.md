# ADR-RT-050-KMCP — Verification Key Management Control Plane

**Status:** Accepted architecture; provider selection pending; implementation is in staged Draft PRs  
**Scope:** Administrative lifecycle of Ed25519 verification/signing keys used by RT-050  
**Related contract:** [ADR-RT-050-RUST-CONTEXT-ISSUER.md](./ADR-RT-050-RUST-CONTEXT-ISSUER.md)  
**SQL implementation candidate:** qembed-sql PR #69 (prototype only; not production-approved)

## 1. Decision

Introduce a dedicated Key Management Control Plane (KMCP) that is outside the request path and owns administrative key lifecycle operations.

The system has three separate security domains:

1. **Key Management Control Plane** — authenticates administrators and automation, stages and activates keys, coordinates rotation, retires/revokes keys, and records audit evidence.
2. **Signer** — exposes a narrowly scoped signing capability to the trusted Rust ContextIssuer. It does not administer the registry and cannot activate, retire, or revoke keys.
3. **PostgreSQL verifier** — verifies signed context tokens using public keys and current key lifecycle state. The application runtime cannot read or mutate the key registry.

No request handler, DTO, tenant Principal, Caller, or ordinary application API may call key-management operations.

The control plane is not called for each token. A control-plane outage must not interrupt verification or signing with an already provisioned, active key. An unavailable signer fails closed; there is no unsigned or legacy-GUC fallback.

## 2. Trust boundaries and identities

### 2.1 Separate identities

| Identity | Purpose | Must not be able to |
|---|---|---|
| Runtime DB role | Execute tenant-scoped work and verified-context accessors | Read/write key registry; execute key-management functions |
| Signer identity | Sign RT-050 payloads with one explicitly assigned key | Register/activate/retire/revoke keys; read arbitrary key inventory |
| KMCP service identity | Call narrowly scoped administrative database functions | Read/write registry tables directly; sign runtime context tokens |
| Audit Exporter identity | Poll pending audit-export events and acknowledge external sink delivery | Read/write audit or outbox tables directly; manage keys or sign runtime context tokens |
| Human Key Operator | Stage a key and initiate a planned rotation | Directly access database tables or private key material |
| Human Key Approver | Approve activation and planned retirement | Export private key material |
| Break-glass Revoker | Revoke a compromised key during an incident | Reactivate or replace a key |

Human administration uses the organization's authenticated identity provider with MFA. The exact OIDC/SSO provider and role-mapping mechanism remain deployment decisions.

Planned activation and retirement require separation of duties: the initiating operator and approver must be distinct authenticated subjects. Emergency revocation is permitted to one authorized break-glass operator to avoid delaying containment; it requires an incident reference and mandatory post-incident review.

The signer identity is workload identity, not a human identity. It is scoped to a deployment, issuer, audience, and explicit key ID.

### 2.2 PostgreSQL role model

Production SQL must create distinct roles:

- `qembed_key_registry_owner` — `NOLOGIN`; owns the registry and SECURITY DEFINER management functions.
- `qembed_key_control_plane` — dedicated login identity for the KMCP service; no direct table DML; EXECUTE only on the approved management and metadata-reader functions.
- `qembed_key_audit_exporter` — separate login identity for the audit outbox dispatcher; no direct audit/outbox table access; EXECUTE only on pending-event, failure-recording, and delivery-acknowledgement functions.
- `qembed_runtime` — application runtime role; verified-context access only; no registry read or lifecycle-function execution.

Management functions must:
- be owned by the NOLOGIN owner role;
- use a fixed, safe `search_path`;
- validate all parameters and allowed state transitions;
- write an audit row in the same database transaction as the state change;
- be executable only by the dedicated KMCP database identity;
- not trust caller-supplied session GUCs as proof of administrative identity.

The database records the authenticated database service identity (`session_user`), request ID, actor subject, approval reference, reason, and deployment ID. The actor subject is asserted by the KMCP after OIDC authentication; PostgreSQL does not independently validate the external identity token. The KMCP must also emit the same event to an independently controlled, append-only audit sink. A database audit row alone is not sufficient evidence against compromise of the KMCP service.

Runtime roles must never be members of the control-plane or audit-exporter roles, directly or through inherited role membership. The control-plane and exporter identities are also separate from each other.

## 3. Production key lifecycle

The SQL PR #69 feasibility fixture already models `staged`, `active`, `retired`, and `revoked`. It remains a prototype; production canonical-schema adoption and independent release gates are still pending.

| State | May sign | May verify | Meaning |
|---|---:|---:|---|
| `staged` | No | No | Public key registered and proof-of-possession checked; awaiting explicit activation |
| `active` | Yes | Yes | Key is approved for new issuance and verification |
| `retired` | No | Bounded | No new issuance; pre-retirement tokens may finish within the grace window |
| `revoked` | No | No | Emergency or final revocation; all tokens rejected immediately |

Allowed transitions:

- `staged -> active`
- `staged -> revoked` (discard a staged key)
- `active -> retired`
- `active -> revoked` (emergency)
- `retired -> revoked`

Reactivation, public-key replacement, identity edits, key-ID reuse, and registry-row deletion are forbidden. Public metadata and lifecycle audit evidence are retained. Private-key destruction is a separate custody operation performed by the key provider under its own retention and recovery policy.

Verification semantics:
- `active`: token `issued_at` must be at or after `activated_at`.
- `retired`: token `issued_at` must be at or before `retired_at`; current database time must be before `verify_until`; and token `expires_at` must not exceed `verify_until`.
- `revoked`: reject every token, regardless of token expiry.
- `staged`, unknown, or absent key: reject.

The existing 120-second default retirement grace and 60-second maximum token lifetime remain the initial protocol policy. The retirement function must enforce the same lower and upper bounds as the approved RT-050 contract. A production migration must explicitly implement the `staged` state rather than silently treating an inserted key as active.

Key IDs are immutable and never reused within a deployment registry. Production should use a distinct Ed25519 key per deployment (not one shared private key across unrelated clusters). The public-key row is bound to the exact issuer, audience, and deployment ID carried in the signed token.

## 4. Key creation and registration

1. KMCP authenticates the operator/workflow and checks authorization, approval policy, deployment scope, and idempotency key.
2. The configured key provider generates a new Ed25519 key pair. Private material is non-exportable where the selected HSM/provider supports the required algorithm and operations.
3. KMCP obtains the public key and an opaque provider key reference. The provider reference is stored in the KMCP's protected inventory, not in application configuration or a tenant-visible database table.
4. KMCP creates a random, short-lived proof-of-possession challenge.
5. The key provider signs the challenge. KMCP verifies the signature against the proposed public key and expected algorithm.
6. KMCP calls the database `stage_verification_key` function with key ID, public key, issuer, audience, deployment ID, actor, request ID, and approval evidence.
7. The database inserts the key as `staged`. It is not accepted by the verifier and cannot be used for successful runtime issuance.

A key is never registered as active merely because a row was inserted. Registration and activation are separate auditable operations.

## 5. Signer boundary and request-path constraint

The RT-050 transaction flow reads backend PID, XID, and database OID and resolves Caller delegation while holding database locks. Consequently, a remote KMS/HSM network round trip for every signature would add an unbounded external dependency while those locks are held.

Therefore:
- The per-token signing operation must use a local, bounded signing capability (for example, a local HSM/PKCS#11 device or a dedicated local signer agent).
- Remote KMS operations may be used for key creation, provisioning, wrapping/unwrapping, and lifecycle administration outside the transaction path.
- A remote-only `Sign` API must not be wired directly into `ContextIssuer::issue` unless the transaction/locking design is changed and a bounded latency/failure contract is separately approved.
- The signer receives exact canonical JCS bytes and returns a detached Ed25519 signature. It does not construct claims, choose tenant/Principal/Caller identity, or set PostgreSQL GUCs.
- The signer is configured for one deployment and a small explicit set of key IDs. It must not be a general-purpose signing oracle exposed over a network.
- Private key material must never enter PostgreSQL, SQL parameters, GUCs, logs, traces, crash reports, or ordinary application configuration.

The Rust API should depend on a narrow signing capability rather than the concrete `RingEd25519SigningKey` type. The `ContextIssuer` now depends on the public `ContextSigner` capability, merged in Rust PR #108. `RingEd25519SigningKey` remains the current in-process implementation. A future HSM/agent adapter must preserve exact-byte signing and key-ID matching. This abstraction does not authorize a remote network call inside the transaction.

If the selected provider cannot offer a bounded local signing path for Ed25519, provider selection is blocked pending an explicit architecture decision; do not silently fall back to a network KMS call.

## 6. Planned rotation protocol

Rotation is a coordinated state transition, not simply changing an environment variable.

1. **Stage:** create the new key in the provider, prove possession, and register its public key as `staged`.
2. **Prepare signers:** distribute the new key reference and key ID to the deployment's signer instances. Each instance loads the capability but does not yet issue with it. Instances report readiness and the loaded key ID to KMCP.
3. **Activate:** after approval and readiness policy succeeds, activate the new public key in PostgreSQL. The verifier can now accept tokens from it.
4. **Switch issuance:** instruct signer instances to use the new key. Verify deployment-wide adoption using instance readiness/heartbeat records.
5. **Drain old issuance:** disable the old key on all signer instances. Wait at least the approved 30-second clock-skew drain interval after the last possible old-key issuance.
6. **Retire:** move the old registry entry to `retired`. Pre-retirement tokens remain acceptable only under the bounded grace and expiry rules.
7. **Finalize:** after `verify_until`, destroy or disable the old private key according to provider retention policy. Keep public registry metadata and audit events.

The new key is activated before signers switch to it, so a signer never emits a token that the verifier cannot yet accept. During overlap, both old and new public keys may be active.

State transitions are monotonic. Before retirement, a failed rollout may return signers to the still-active old key. After retirement, the old key cannot be reactivated; recovery is forward-only using a new staged key.

All operations are idempotent by request ID and use compare-and-set state transitions under a row lock. Concurrent activation, retirement, and revocation attempts must serialize and return an explicit conflict rather than overwrite a newer state.

## 7. Emergency revocation and multi-deployment propagation

Emergency revocation has no grace period.

1. Authenticate a break-glass operator and require an incident reference.
2. Revoke the key in the affected deployment's PostgreSQL registry immediately.
3. Disable the corresponding signing capability at the signer/provider.
4. Propagate the revocation to every other deployment whose inventory references the same key (shared key material should be avoided).
5. Record acknowledgements from each deployment and emit an immutable audit event.
6. Rotate to a new key through the normal staged/activation process.

The database in which revocation commits must reject the key on subsequent READ COMMITTED statement snapshots. The signed-context transaction entry point must enforce READ COMMITTED. This is a per-database guarantee, not a claim of instantaneous global propagation.

KMCP reports global revocation as complete only after every registered target deployment has acknowledged the revocation. Until then the operation is `propagating`/degraded, and the operator receives the list of unacknowledged deployments. Incident procedures must define how an unreachable deployment is isolated or prevented from serving authenticated traffic.

If KMCP is unavailable, no new lifecycle operation can be assumed successful. Existing active keys continue to work until their configured policy says otherwise; this is not fail-open verification. For a suspected compromise during a control-plane outage, operators must use the documented break-glass path to disable the signer and isolate affected database deployments.

## 8. Administrative API and authorization

The first control-plane API is intentionally small:

- `POST /v1/deployments/{deployment_id}/keys:stage`
- `POST /v1/deployments/{deployment_id}/keys/{key_id}:activate`
- `POST /v1/deployments/{deployment_id}/keys/{key_id}:retire`
- `POST /v1/deployments/{deployment_id}/keys/{key_id}:revoke`
- `GET /v1/deployments/{deployment_id}/keys`
- `GET /v1/operations/{request_id}`

Every mutation requires an idempotency/request ID, authenticated actor subject, reason, and (except emergency revoke) a distinct approver reference. The service enforces deployment-scoped RBAC and dual control. Read operations return public metadata only; no endpoint returns private key material or raw provider credentials.

These are proposed resource/action names, not a commitment to a particular HTTP framework or API version. The control plane is an administrative service and does not alter the Owner API or runtime API contracts.

## 9. Audit and observability

Every mutation emits:
- immutable event ID and idempotency/request ID;
- deployment ID, key ID, prior state, and new state;
- operation and timestamp;
- authenticated actor subject and approver subject (where required);
- KMCP workload identity and database `session_user`;
- reason/incident reference and approval reference;
- provider key reference fingerprint (never secret material);
- propagation targets and acknowledgements.

The database audit table is append-only. Each lifecycle function inserts both the audit event and a transactional audit-export outbox record in the **same database transaction** as the key state change. An independent exporter, using its own restricted database identity and SECURITY DEFINER read/acknowledgement functions, delivers events to an independently administered immutable audit sink. Delivery is at-least-once and idempotent by event ID; the exporter records delivery acknowledgement separately from the immutable audit event.

No network call to the external audit sink occurs while key-registry locks are held. If the sink is unavailable, the key state change and local audit remain durable while export stays visibly pending and is retried. Emergency revocation must not wait for the external sink; the control plane reports local revocation separately from audit-export completion. If the outbox record cannot be committed, the key state transition rolls back.

Logs must not contain private key material, signed payloads, raw tokens, or credentials. Metrics cover staged/active/retired/revoked counts, signer key adoption, rotation duration, revocation propagation lag, audit-export backlog/age, failed authorization, and rejected lifecycle transitions.

## 10. Failure behavior

- KMCP unavailable: no administrative change is assumed to have succeeded; existing verification and signing continue with already provisioned active keys.
- Signer unavailable or key not loaded: ContextIssuer fails closed; no unsigned/legacy fallback.
- Key provider unavailable during staging: do not create or activate a partial key; operation remains failed/retryable with the same idempotency key.
- Database unavailable during lifecycle mutation: operation is not marked complete; retry uses the same request ID and re-reads state.
- Partial multi-deployment revocation: local committed revocations remain effective; global operation remains incomplete and exposes outstanding targets.
- Unknown key, staged key, retired key outside grace, revoked key, wrong deployment/audience/issuer: verifier rejects.
- Control-plane or signer outage must never cause runtime to infer that a key is active or bypass PostgreSQL verification.

## 11. Implementation sequence

1. **SQL registry hardening:** add `staged`, management/audit tables, NOLOGIN owner role, dedicated KMCP role, SECURITY DEFINER functions, grants, and least-privilege tests. Keep runtime unable to read or mutate registry.
2. **Rust signer boundary:** decouple `ContextIssuer` from the concrete ring key while preserving the bounded local signing contract; keep ring as the initial implementation.
3. **KMCP service contract:** define authenticated operator/approver identity, deployment-scoped RBAC, idempotent operations, and external audit events.
4. **Provider adapter:** select and implement the concrete Ed25519 key provider and local signer integration only after verifying algorithm support, custody guarantees, operational latency, and disaster recovery.
5. **Integration and adversarial tests:** exercise staged-to-active, rotation overlap, stale signer rejection, retirement grace, immediate revoke, concurrent admin operations, partial multi-cluster propagation, audit atomicity, and signer/KMCP outages.

SQL PR #69 remains a feasibility prototype and is not the production registry migration. Its four-state fixture is more complete than the earlier three-state design, but it does not substitute for canonical-schema adoption, external key custody, production KMCP authentication or rollout evidence.

## 12. Decisions intentionally left open

The architecture does not yet select:
- a cloud KMS/HSM vendor or on-premises HSM;
- whether the local signing capability is a PKCS#11 HSM integration or a local signer agent with protected key loading;
- the organization's OIDC/SSO provider and operator group mapping;
- the immutable audit sink;
- the production PostgreSQL owner/control-plane role provisioning mechanism;
- deployment inventory and revocation propagation transport.

These choices require an implementation environment and operational constraints. They do not change the trust boundaries and lifecycle invariants defined above.

## 13. Acceptance criteria

- Runtime DB credentials cannot read registry/audit tables or execute management functions.
- KMCP DB credentials cannot directly select/insert/update/delete registry rows; only approved functions are executable.
- Audit-exporter DB credentials cannot directly select/update/delete audit or outbox rows; only pending-event, failure-recording, and acknowledgement functions are executable.
- Registry owner and function owner are NOLOGIN roles.
- New keys are staged and are never verifiable before explicit activation.
- Proof-of-possession is checked before staging completes.
- Key identity/material cannot be replaced, key IDs cannot be reused, and rows cannot be deleted.
- All lifecycle transitions are monotonic, idempotent, serialized, and audited atomically.
- Each lifecycle transaction writes an append-only audit event and export-outbox record atomically with the key state change.
- Audit export uses a separate restricted identity, at-least-once delivery, idempotent event IDs, explicit acknowledgements, and observable retry/backlog state; external network delivery never runs under key-registry locks.
- Planned activation/retirement enforce distinct operator and approver identities.
- Emergency revoke requires break-glass authorization and incident reference, has no grace period, and records propagation acknowledgements.
- Per-token signing does not call a remote KMS over the network while PostgreSQL delegation locks are held.
- Signer access is deployment/key scoped; private material never enters PostgreSQL or application logs/configuration.
- Multi-deployment revocation is not reported complete until all registered targets acknowledge.
- KMCP/signer/provider outages fail closed and never activate an unverified key or bypass the verifier.
- Rotation and emergency revocation are covered by Rust/PostgreSQL integration tests and an independent security review before production rollout.
