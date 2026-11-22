# RT-050 — Rust ContextIssuer and PostgreSQL transaction API

**Status:** Implementation contract draft  
**Verifier candidate:** `qembed/qembed-sql#69` (explicitly selected for RT-050 feasibility work; not production-approved)  
**Scope:** Rust transaction lifecycle and the matching PostgreSQL signed-context contract

## 1. Decision boundary

The project owner selected the minimal Qembed-maintained Ed25519 C verifier from SQL PR #69 as the implementation candidate. This is not production approval. Independent security review, fuzzing, packaging/provenance work, supported PostgreSQL-version coverage, production RLS migration and representative workload benchmarks remain release gates.

The Rust issuer signs; PostgreSQL only verifies. No SQL function, database role, or runtime SQL caller receives a signing capability. The private key remains outside PostgreSQL.

The pgsodium prototype in SQL PR #68 is not the selected implementation for this work.

## 2. Signed token wire contract

The current SQL prototype signs the exact JSON payload bytes and verifies them before exposing claims. Rust and SQL must use one canonical byte representation; parsing and re-serializing before signature verification is forbidden.

### 2.1 Canonical payload

- UTF-8 JSON serialized using RFC 8785 JSON Canonicalization Scheme (JCS).
- Rust uses the workspace-pinned `serde_jcs` implementation.
- The exact canonical byte sequence is signed with Ed25519 detached signing.
- Maximum payload size: 8 KiB, matching the verifier prototype.
- Signature: exactly 64 bytes; public key: exactly 32 bytes.
- Claims are strings unless a later versioned contract explicitly changes a field type. This avoids cross-language number formatting ambiguity.

Required version-1 claims:

| Claim | Meaning |
|---|---|
| `version` | Literal protocol version `1` |
| `tenant_id` | Authenticated tenant UUID |
| `principal_id` | Authenticated Principal UUID |
| `caller_id` | Exact Caller resolved from active direct delegation |
| `backend_pid` | PostgreSQL backend PID obtained from the active transaction |
| `transaction_id` | `pg_current_xact_id()::text` from that transaction |
| `database_oid` | Current database OID obtained from PostgreSQL |
| `deployment_id` | Stable deployment identity from trusted server configuration |
| `issuer` | Configured ContextIssuer identifier |
| `audience` | Exact PostgreSQL deployment/database audience |
| `key_id` | Identifier of the Ed25519 public key registered in PostgreSQL |
| `issued_at` | UTC RFC 3339 timestamp |
| `expires_at` | UTC RFC 3339 timestamp; lifetime must not exceed 60 seconds |
| `nonce` | Random UUID for correlation; not claimed as independent replay protection |

The issuer defaults to a 30-second token lifetime. PostgreSQL enforces expiry, a maximum 60-second lifetime, and at most 30 seconds of future clock skew. A nonce is not a substitute for transaction binding or an atomic one-time-use mechanism.

### 2.2 Single-setting envelope

The Rust transaction API installs exactly one transaction-local setting:

`app.context_token = qctx1:<payload-hex>:<signature-hex>`

- `qctx1` is the envelope version.
- Hex is lowercase and contains no separators other than the two literal colons.
- Payload and signature are the exact byte arrays described above.
- PostgreSQL rejects malformed envelopes, unknown envelope versions, invalid hex, oversized payloads, and signatures of the wrong length.
- The SQL verifier extracts `key_id` only to select a candidate public key, verifies the original payload bytes, and only then trusts/parses the claims.
- The GUC is mutable and visible to SQL. Its security comes solely from the signature and backend/transaction/database binding, not from secrecy or immutability.

## 2.3 Verification-key lifecycle

The PostgreSQL verification-key registry is the authority for whether a key may validate a token. Version 1 uses three monotonic states:

| State | Verification behavior |
|---|---|
| `active` | Accept a valid token issued at or after `activated_at`. |
| `retired` | Do not accept tokens issued after `retired_at`. Accept earlier tokens only while `now < verify_until` and only when `expires_at <= verify_until`. |
| `revoked` | Reject every token signed by the key, including tokens that have not expired. |

Allowed transitions are `active -> retired -> revoked` and `active -> revoked` for emergency response. Reactivation, key-material replacement, identity edits and deletion of registry rows are forbidden. Public-key rows remain as audit evidence; private-key destruction is a separate custody operation.

The retirement grace defaults to **120 seconds** and may be configured up to one hour. This exceeds the 60-second maximum token lifetime plus 30 seconds of permitted future clock skew, with an additional margin. A retired key is verification-only; a stale Rust instance may still produce a signature locally, but PostgreSQL rejects a token whose `issued_at` is after `retired_at`.

### Planned rotation sequence

1. Register the new public key as `active` while the old key remains active.
2. Deploy the new signing key/configuration to every Rust issuer instance and verify rollout completion.
3. Stop all issuance with the old key. Wait at least 30 seconds after the last possible old-key issuance so permitted positive clock skew cannot place a pre-cutover token after the database retirement timestamp.
4. Retire the old key. Pre-retirement tokens may finish only within the bounded grace and their own expiry; no post-retirement token is accepted.
5. After `verify_until`, remove/destroy the old private key according to the approved secret-store backup and retention policy. Keep the public registry row for audit.

Emergency revocation transitions an active or retired key directly to `revoked`; it has no grace period. Signed-context transactions explicitly use PostgreSQL **READ COMMITTED** isolation so each new statement obtains a fresh snapshot and observes a committed revocation. A transaction deliberately running under a repeatable-read snapshot could continue to see the old key row; the signed-context entry point must therefore enforce READ COMMITTED rather than inherit a pool/session default.

The SQL prototype currently uses separate payload/signature settings. Its test harness must be adapted to this single-setting contract before Rust/SQL integration is considered complete.

## 3. Rust ownership and API boundaries

The implementation belongs in `qembed-store`, because it owns the PostgreSQL transaction and SQLx connection lifecycle. HTTP handlers and request DTOs must not construct claims or tokens.

The public transaction entry point is conceptually:

```rust
begin_context_transaction(
    pool: &PgPool,
    scope: &Scope,
    requested_caller_id: CallerId,
    issuer: &ContextIssuer,
) -> Result<ContextTransaction, ContextTransactionError>
```

The API is intentionally not a generic token-signing API. It accepts trusted `Scope` and a requested Caller ID; it does not accept tenant, Principal, backend PID, transaction ID, database OID, timestamps, key ID, claims, or raw token bytes from the caller.

`ContextIssuer` receives only:
- the trusted `Scope`;
- a `ResolvedCaller` minted by the PostgreSQL delegation resolver and bound to the same transaction;
- a private transaction-binding value read from the active PostgreSQL connection;
- trusted deployment/issuer/audience/key configuration.

A concrete Ed25519 signer is injected behind a narrow signing-key provider boundary. Key loading/KMS/HSM integration is deliberately not inferred from the verifier choice and must not place private key material in PostgreSQL.

## 4. Transaction lifecycle and ordering

1. Begin a PostgreSQL transaction on one pooled connection.
2. Establish the trusted bootstrap selector needed by the existing delegation resolver from `Scope`. These legacy settings are transitional lookup inputs only; they are not authorization evidence and must not be read by production RLS policies after this migration.
3. Read backend PID, transaction ID and database OID from that same connection and transaction.
4. Resolve the exact requested Caller using the existing direct-delegation database check. Preserve its row locks through the transaction so concurrent revocation/deactivation remains serialized with invocation start.
5. Build the version-1 claims from `Scope`, `ResolvedCaller`, PostgreSQL binding and trusted issuer configuration. Sign canonical payload bytes.
6. Set only `app.context_token` as the signed authorization context. All production RLS/security helpers consume verified claims through the SQL verifier; raw `app.tenant_id`, `app.principal_id` and `app.caller_id` are never fallback authorization inputs.
7. Execute tenant-scoped repository operations and invocation-start work on the same transaction/connection.
8. Commit or roll back. The token becomes unusable for later transactions because its transaction ID no longer matches. Savepoints remain inside the same outer transaction and do not mint a new identity.
9. On any error before commit, roll back and discard the token. A retry starts a fresh transaction, re-reads its binding, re-resolves delegation and issues a new token.

No external network call or KMS round trip should occur while database row locks are held unless the signer implementation is explicitly local and bounded. The key-provider design must therefore make key availability and signing latency predictable; issuer unavailability is fail-closed.

## 5. Failure and security behavior

- Issuer/signing-key unavailable: fail closed; no unsigned or legacy-GUC fallback.
- Unknown, retired-for-new-issuance, revoked, expired or not-yet-valid key: SQL verification fails according to the lifecycle rules in §2.3.
- Wrong backend, transaction, database, deployment, issuer or audience: SQL verification fails.
- Altered claims, altered signature, malformed envelope or payload over 8 KiB: SQL verification fails.
- Delegation revoked concurrently: row-lock ordering determines the winner; retain the existing direct-delegation serialization contract.
- Commit, rollback, connection reuse or transaction retry: old token must fail; a retry obtains a fresh token.
- Savepoint rollback: outer transaction token remains valid.
- Arbitrary SQL may overwrite `app.context_token`, but cannot create a valid replacement without the private key. It may cause denial of service by replacing the setting; it must not gain another identity.
- No claim is made that this protects against compromise of the application process/ContextIssuer, private signing key, PostgreSQL superuser, or privileged database administrators.

## 6. Required SQL companion changes

SQL PR #69 remains a feasibility prototype until the following companion contract is implemented and tested:

- Replace the two test GUCs with the single `app.context_token` envelope.
- Expose a narrowly privileged verified-context function and typed claim accessors for tenant, Principal and Caller.
- Ensure RLS uses an uncorrelated scalar subquery / InitPlan so signature verification is performed once per statement, not once per scanned row.
- Add the active/retired/revoked verification-key lifecycle, monotonic transitions, immutable key material, bounded retirement grace and privileged management functions; runtime cannot read or mutate the registry or execute lifecycle functions.
- Validate all required claims, UUID formats, claim version, issuer/audience/deployment/database/backend/transaction binding, timestamps and maximum lifetime.
- Reject unknown JSON fields or define their forward-compatibility behavior explicitly.
- Keep `qembed_runtime` unable to invoke the low-level C verifier directly or read verification keys.
- Add negative tests for forged legacy GUCs, cross-tenant/Principal/Caller substitution, invalid key state, and connection-pool reuse.
- Keep production RLS migration separate from the feasibility fixture until security review and the supported PostgreSQL matrix are complete.

## 7. Open deployment decisions

These do not change the verifier selection, but must be settled before production:

1. Where the private signing key is held and how Rust obtains it (KMS/HSM/secret manager or an explicitly approved local key source).
2. KMS/HSM key custody, multi-instance rotation orchestration, emergency revocation propagation and public-key distribution. The SQL prototype defines verification semantics, not a production key-management control plane.
3. Stable deployment-ID provisioning and uniqueness across clusters.
4. Whether nonce-based single-use semantics are required beyond exact backend/transaction binding.
5. Supported PostgreSQL versions and extension packaging/provenance policy.

## 8. Acceptance tests

Rust + PostgreSQL integration must prove:

- correct claims are issued only from trusted `Scope`, database binding and database-resolved Caller;
- the private key never enters SQL parameters, GUCs, logs or database storage;
- one connection is used from BEGIN through context install, repository work and COMMIT/ROLLBACK;
- commit/rollback replay fails on the same pooled connection and another backend;
- savepoint rollback preserves the outer context;
- a read-only transaction can obtain the required transaction ID and use context;
- concurrent delegation revocation is serialized with the invocation-start transaction;
- old-key tokens remain valid only during the retirement grace; post-retirement issuance is rejected; emergency revocation invalidates pre-existing tokens on subsequent READ COMMITTED statements;
- runtime cannot read the verification-key registry or execute key lifecycle management functions;
- forged legacy identity GUCs do not affect RLS;
- malformed, altered, expired, wrong-key and wrong-binding tokens fail closed;
- pool reuse cannot expose a previous transaction's identity;
- the SQL verifier is called once per statement in RLS plans.

Production approval additionally requires independent security review, C-extension fuzzing, packaging/provenance review, supported PostgreSQL matrix, production RLS migration, and representative workload performance evidence.
