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
The canonical qembed-sql schema also declares cross-table foreign keys from
`budget_reservations` to `object_invocations` and from `caller_usage_counters` to
`callers`. Those two parent tables are not yet introduced by this partial Rust
migration chain, so their foreign keys remain owned by the canonical schema and
must be added to this chain when the corresponding parent-table migrations land.
