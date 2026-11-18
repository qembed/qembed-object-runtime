# Invocation idempotency reservation

RUST-035 implements the reservation boundary for the M1a `begin-invocation` flow.

## Physical source of truth

The physical contract is defined by the canonical SQL schema in
[qembed/qembed-sql](https://github.com/qembed/qembed-sql/blob/master/qembed_m0_v2_3_3_clean_schema.sql),
not by a new Rust migration. The relevant table is `invocation_idempotency`:

- Primary key: `(tenant_id, object_id, caller_id, idempotency_key)`.
- Unique reservation target: `(tenant_id, object_id, invocation_id)`.
- `payload_hash` stores opaque bytes for request identity comparison.
- `expires_at` is consumed by the SQL retention path.
- The composite FK to `object_invocations(tenant_id, object_id, caller_id, id)`
  is `DEFERRABLE INITIALLY DEFERRED`.

The deferred FK is intentional: the begin-invocation transaction claims the
idempotency key first, then inserts the matching invocation row before commit.
No production DDL or migration is introduced by RUST-035.

## Runtime behavior

`qembed-store::idempotency::reserve_invocation_idempotency` receives an already
tenant-bound `TenantTransaction` and derives `caller_id` from the trusted
`Scope`; callers cannot choose another caller's idempotency namespace.

- A new key returns `Reserved { invocation_id }`. The caller must insert that
  invocation in the same transaction before commit.
- A repeated key with identical `payload_hash` returns
  `Replay { invocation_id }` with the original ID. No second invocation is
  created.
- A repeated key with a different `payload_hash` returns `PayloadMismatch`;
  the caller rolls back and reports an idempotency conflict.
- PostgreSQL's primary key serializes concurrent claims. The losing transaction
  reads the committed reservation and returns the winning invocation ID.
- Rolling back the transaction removes the uncommitted reservation as well.

The payload hash is deliberately opaque here. RUST-035 does not invent request
canonicalization or choose a hash algorithm; the begin-invocation layer must
supply a stable digest for the logical request. Likewise, the absolute expiry is
supplied by the caller; this layer does not invent a retention duration.

An expired key remains occupied until the configured retention process deletes
its row. Expiry is not treated as permission to reuse a key before that cleanup
has actually happened.

## External side-effect boundary

The transaction that commits the idempotency reservation and invocation must
finish before any provider call. Provider I/O is never performed while holding
the SQL transaction. RUST-036 composes this primitive with the pinned snapshot,
knowledge revision, event, and outbox writes in the complete atomic
begin-invocation transaction.

## Tests

PostgreSQL integration tests cover same-key replay, concurrent requests,
payload mismatch, rollback releasing a claim, and isolation across tenant,
object, and authenticated-caller scopes. The isolated test schema reproduces
the relevant canonical table constraints, including the deferred composite FK;
it is test fixture DDL only.