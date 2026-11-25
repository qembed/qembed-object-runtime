# RUST-075 — Owner-authored memory promotion

## Contract

- Only a trusted TenantOwner Scope may promote a candidate. TenantAdmin and Service are mapped to the non-owner Authenticated trust class and are rejected before opening a database transaction.
- Only semantic, Unverified, User or ThirdParty candidates are eligible. Episodic memories cannot be Verified under the canonical SQL constraint.
- Promotion inserts a new semantic row with source_kind=Owner, verification=Verified, verified_by=Scope.principal, verified_at=clock_timestamp(), confidence=1.0, and supersedes_id=candidate.id.
- The candidate is not updated. Its source, verification state, claim, and identity remain unchanged.
- The candidate is locked FOR UPDATE during promotion. Competing calls serialize; a second call detects the existing Owner/Verified superseding row and fails as already promoted.

## Deliberate boundaries

- The new Owner row copies the candidate claim, but does not copy its source ID, invocation ID, Caller ID, expiry, or embeddings. The supersedes edge is the provenance link to the original candidate.
- This is explicit Owner confirmation, not automatic trust elevation or semantic entailment.
- PostgreSQL remains responsible for tenant/object foreign keys, the Verified-row guard, and RLS. The Rust application adds the stricter TenantOwner-only authorization boundary.

## Acceptance

- Domain unit tests cover owner/non-owner, eligible User/ThirdParty candidates, episodic rejection, and invalid source/verification states.
- Canonical PostgreSQL integration proves a new Verified row is created, candidate remains Unverified and unchanged, supersedes_id and verified_by are correct, repeated promotion is rejected, and a non-owner is rejected before SQL.
