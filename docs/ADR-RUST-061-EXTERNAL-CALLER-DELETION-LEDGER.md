# ADR RUST-061: External Caller Deletion Ledger and Restore Replay

- **Status:** Accepted
- **Date:** 2026-11-25
- **Scope:** RUST-061
- **Decision owners:** Qembed Object Runtime architecture

## Context

The canonical SQL model contains `deletion_jobs` and `deletion_ledger` in the Runtime database. A ledger restored from the same backup as Caller-derived data is not an independent erasure record: restoring an older backup can roll back both the data deletion and its evidence.

Acceptance requirements T-10, T-35 and T-48 require Caller-derived data to remain erased after restore, and require replay to finish before traffic is admitted. A ledger in the Runtime backup domain cannot prove this property.

## Decision

The service uses **one mandatory External Deletion Ledger** in a dedicated Control-plane PostgreSQL cluster with an independent backup, recovery and access-control domain.

There is no local-only mode, no configuration switch, and no fallback to a Runtime-local ledger. The Control-plane is a required dependency at startup and readiness.

The external ledger is the sole authoritative source of Caller deletion tombstones. Runtime may retain ordinary Caller lifecycle state (for example, `callers.revoked_at`) but must not treat a Runtime-local ledger or projection as an authoritative substitute.

## Data minimization and tombstone identity

The Control-plane stores only the minimum information required to recognize an erased Caller:

- `tenant_id`;
- a 32-byte HMAC-SHA-256 digest of the tenant-scoped Caller identity;
- a fixed digest/key version identifier;
- `deleted_at`.

It must not store Caller tokens, credentials, names, messages, memories, tool payloads, or other Caller content. Raw Caller IDs are not persisted in the Control-plane ledger.

The digest input is domain-separated and tenant-bound:

`HMAC-SHA-256(K_v1, "qembed/caller-deletion/v1" || 0x00 || tenant_uuid_bytes || caller_uuid_bytes)`.

The HMAC key is supplied through deployment secret management, never stored in either PostgreSQL database or in a database backup. The key is required for normal deletion and restore replay. Loss of the key makes the ledger unusable and is therefore a critical recovery failure. Key rotation requires retaining old verification keys and a separate migration/ADR; RUST-061 initially supports the v1 key only.

The ledger has a unique key on `(tenant_id, caller_hash)`. Repeated deletion is idempotent and must not change the original tombstone timestamp. Rows are append-only: application credentials may insert and read, but may not update or delete.

## Deletion protocol

Deletion is an idempotent, retryable state machine:

1. In Runtime PostgreSQL, validate that the Caller belongs to the authenticated tenant and irreversibly revoke it. This is a lifecycle block, not data erasure; it prevents new invocation paths from accepting the Caller while the external write is pending.
2. Commit that Runtime transaction. Do not hold a Runtime SQL transaction open across a network call.
3. Insert the HMAC tombstone into the Control-plane ledger and wait for its durable commit acknowledgement. On conflict, preserve the existing tombstone and treat the operation as already recorded.
4. Only after the external tombstone is durable, delete Caller-derived Runtime data and mark the deletion job complete.
5. If any step after revocation fails, keep the Caller revoked and the job retryable. Never report deletion complete until both the external tombstone and Runtime cleanup have succeeded.

No network call to the Control-plane is made inside a Runtime PostgreSQL transaction. Control-plane unavailability fails closed: no local fallback and no successful deletion acknowledgement.

## Restore protocol

A Runtime restore is a maintenance operation, not a normal service start:

1. Keep application readiness false and do not admit business traffic.
2. Connect to the mandatory Control-plane and read a consistent snapshot of the complete append-only tombstone set (including its high-water mark).
3. Reconcile every tombstone against Caller identities in the restored Runtime database using the retained HMAC key. For each match, re-apply Caller revocation and idempotently erase all Caller-derived rows.
4. Verify that every tombstone in the snapshot has been processed and that no matching Caller-derived rows remain.
5. Re-check the Control-plane high-water mark while holding a session-level advisory lock that blocks all tombstone writers. A mismatch is a replay failure; it must never be ignored.
6. Return a private replay-complete capability that retains this lock. The first successful readiness check releases it. The API composition root cannot construct a readiness probe without this capability.

The restore/replay implementation uses a separate Runtime-database login that inherits only the `qembed_restore` role. That role can execute the HMAC replay function but has no direct table access. It must not weaken ordinary tenant RLS or grant the normal Runtime application role unrestricted cross-tenant access. If an invocation or tool execution is still non-terminal, replay requests cancellation and fails closed; startup remains not ready until the work reaches a terminal state and replay is retried.

## Availability and readiness

- Control-plane PostgreSQL is a mandatory dependency. Missing configuration, connection failure, migration mismatch, or inability to read/append tombstones makes the service not ready.
- Liveness remains independent of database availability.
- No automatic downgrade to local storage is permitted.
- Control-plane requires independent HA, backups/PITR, restricted credentials, and tested recovery. Replication alone is not a backup.
- If both the Control-plane and all of its recoverable backups are lost, the anti-resurrection guarantee cannot be established. This is an explicit infrastructure-level critical dependency.

## Consequences

- The service needs a second PostgreSQL pool and a dedicated Control-plane schema/migration lifecycle.
- Deployment configuration must provide `CONTROL_PLANE_DATABASE_URL`, `RESTORE_DATABASE_URL` (a restricted Runtime login), and the v1 HMAC secret.
- Runtime readiness includes both databases and the restore replay barrier.
- Caller deletion and restore replay become explicitly fail-closed operations.
- The canonical SQL repository must define the external ledger schema and the narrowly privileged Runtime reconciliation capability; the Rust repository implements the adapters and orchestration.
- The local `deletion_ledger` table in the existing canonical Runtime schema is not authoritative and must not be used as a fallback. Its eventual removal is a SQL-repository migration decision, not an implicit Rust runtime behavior.

## Acceptance tests

- A Caller is revoked before any external network operation; a failed Control-plane write leaves it revoked and the deletion job retryable.
- A successful tombstone commit precedes physical cleanup.
- Repeated deletion preserves one tombstone and completes cleanup idempotently.
- Control-plane outage never falls back to Runtime-local state and makes readiness fail.
- Restoring a Runtime backup from before deletion and replaying the external ledger removes the Caller-derived data again.
- Traffic remains unavailable until replay is complete and its high-water mark is stable.
- Control-plane credentials cannot update/delete tombstones; the Runtime role cannot access Control-plane data except through its narrow adapter credentials.
