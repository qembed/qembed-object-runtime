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
versions. Composite foreign keys keep every version attached to an object in
the same tenant; database uniqueness constraints prevent duplicate object IDs
and per-object version numbers.

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
