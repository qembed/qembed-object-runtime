# Store migrations

SQLx migrations for this crate belong in this directory and use the standard
`<VERSION>_<description>.sql` naming convention.

The migration runner is explicit: connecting to PostgreSQL never runs migrations
as a side effect.

The owner-authentication migration persists tenant-bound owner principals and
SHA-256 digests of high-entropy API tokens. Raw tokens are never stored. The
digest is appropriate for random API credentials, not for human passwords;
password authentication would require a dedicated password-hashing scheme.

The object repository migration adds tenant-owned objects and numbered object
versions. Object IDs are unique within a tenant; ObjectVersion IDs are unique
within their tenant/object pair. Composite primary and foreign keys preserve
those scopes and prevent cross-tenant or cross-object references. These UUIDs
are deliberately not globally unique: a uniqueness error must not reveal that
another tenant already uses an identifier.

The snapshot migration stores the exact canonical JSON byte sequence in
`snapshots.canonical_json BYTEA`. Its SHA-256 digest is stored separately in
`content_hash BYTEA`, while `json_projection JSONB` is derived from those
bytes and checked by a database trigger. Snapshot rows reject UPDATE and
DELETE; the repository verifies the stored digest again when reconstructing a
domain Snapshot. JSONB is never used to recreate canonical bytes.

The RUST-041b migration adds token-budget accounting tables and tenant RLS policies.
The RUST-041d migration adds one durable `provider_call_attempts` row for each
new token reservation. The trigger creates the `NotStarted` row in the same
transaction as the reservation insert. State transitions and immutable
response-usage rules are enforced by PostgreSQL, and the table uses the trusted
tenant setting for RLS.

The RUST-044 invocation-lifecycle migration completes the persistence slice
required by the current Rust invocation path. It adds the canonical tenant and
Caller/delegation records, the `resolve_delegated_caller` SECURITY DEFINER
function, `object_invocations`, `invocation_idempotency`,
`interaction_messages`, and `knowledge_revisions`. It also installs the
Object-to-knowledge-revision initializer and completes the deferred foreign
keys from budget reservations and provider-call attempts to invocations, plus
the caller usage counter foreign key to Callers.

The invocation and idempotency foreign keys intentionally remain deferred where
the begin-invocation transaction first reserves the idempotency key and budget,
then inserts the invocation, and commits all related rows atomically. The
migration follows the corresponding definitions in the canonical
`qembed/qembed-sql` schema; it does not change that repository.

The RUST-053 ToolExecution migration adds the minimal canonical prerequisites
`tool_definitions`, `tool_versions`, and `policy_decisions`, then installs
`tool_executions` with its own optimistic version, fencing token, lease owner,
expiry and heartbeat. The initial claim advances only the ToolExecution-owned
version/token; heartbeats retain both; result writes require the current live
ToolExecution lease. Expired executions enter `Unknown` and require explicit
reconciliation rather than automatic replay. Invocation lease metadata is not
used as authority for ToolExecution writes.

This migration chain is the implemented Rust persistence slice, not a copy of
every future table in the full canonical schema. Tables without a current Rust
adapter or approved implementation scope remain outside this chain until their
corresponding work is scheduled.

The RUST-052c migration adds a durable replay marker to an already-published
Outbox row and a tenant-scoped PostgreSQL recovery function. The sweeper only
marks stale Received invocations with no active lease and safe provider-attempt
evidence. The Relay republishes the same delivery ID; successful PubAck clears
the pending marker while appending to the immutable publication receipt history.
Ambiguous provider outcomes fail closed and are left to reconciliation.


Snapshot IDs and Event IDs are likewise tenant-scoped by their composite primary keys. Outbox references include tenant_id, so no global uniqueness on either UUID is required. Runtime lookups must always carry the authenticated tenant scope.


The RUST-058c migration introduces the canonical tenant Principal registry into
the Rust persistence slice and moves Caller-delegation foreign keys from the
legacy Owner table to `principals(tenant_id, id)`. Existing Owner API tokens
remain verified by `owner_credentials`; their stable identities are mirrored
into `principals` with a synthetic, non-authenticating digest. New Principal
credentials are verified through the SECURITY DEFINER
`authenticate_principal(bytea)` function. SQL roles `TenantAdmin` and
`Service` map to the runtime's `Authenticated` trust class; global
`PlatformAdmin` identities cannot establish a tenant Scope.

A TenantOwner grants or revokes a Caller for a target Principal within the
same tenant. Invocation-time resolution locks the target Principal, Caller and
delegation together. The migration also adds
`objects.anonymous_invocation_enabled BOOLEAN NOT NULL DEFAULT TRUE`; the
setting is per Object and does not grant access to an entire Tenant. HTTP
admission and replay-aware Valkey integration are separate implementation
steps and are not implied by this schema migration alone.


The Anonymous public-invocation adapter depends on the canonical qctx2 verifier
and Object-scoped Anonymous Caller functions from `qembed/qembed-sql`
(RT-059 and RT-060). Those security-definer functions are deliberately not
copied into this Rust migration chain: the SQL repository remains their single
source of truth. `scripts/test-postgres.sh` pins the merged SQL revision and
applies RT-059/RT-060 to the canonical integration database before running the
qctx2 PostgreSQL tests.


The RUST-059 kill-switch adapter reads the canonical `kill_switches` table
from `qembed-sql`. It intentionally adds no Rust-owned copy of that table:
scope constraints, management-authority enforcement, grants and RLS remain
defined by the canonical SQL schema. Runtime evaluation is deny-dominant:
any matching disabled Platform, Tenant, Object or Tool switch blocks the
side effect before it starts; lower-level enabled switches cannot override
an ancestor disable.


The RUST-066 Knowledge migration adds the Store-owned persistence slice for
revision commits: the active embedding-model registry projection, revision log,
documents, chunks, vector embeddings, tenant RLS policies, append-only log
guard, and the definer-owned document-to-chunk visibility synchronization
trigger. The commit repository locks the revision row and model registry,
checks that every indexed document has chunks and every chunk has an embedding
for every active model, then changes document/chunk intervals and appends the
revision log, event and outbox row in one transaction. The full production
Knowledge schema remains canonical in `qembed/qembed-sql`; this is the
incremental Rust integration-test/runtime slice.
