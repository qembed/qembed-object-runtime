# RUST-044 — Owner API and trusted publication validation

**Status:** architecture baseline recorded; implementation contract is constrained by the surviving M0 artifacts and the explicit specification gaps in [M0_SPEC_GAPS.md](./M0_SPEC_GAPS.md).

## 1. Scope and source hierarchy

RUST-044 is the M1a Owner API walking-skeleton slice: create object/version, publish, invoke, inspect invocation. The Owner API is a transport boundary, not a publication authority.

The original M0 v2.2 source specification is unavailable and must be treated as lost. It has not been reconstructed. No implementation or documentation may claim to have reviewed, recovered, or inferred requirements from that missing source.

The verifiable normative baseline is limited to the surviving M0 v2.3.3 r3 Finalization Addendum, ADR RT-039…RT-048, acceptance T-01…T-64, and canonical SQL artifacts in qembed-sql. The consolidated PDF is marked r1 and was not re-rendered after r3; the Markdown addendum is authoritative for the changes it explicitly covers. These surviving artifacts do not establish that every original M0 v2.2 rule is present.

Any requirement not established by the surviving artifacts is a specification gap. If the project later chooses a rule to fill such a gap, record it as a new, explicitly approved project decision/ADR—not as recovered M0 behaviour. See [M0_SPEC_GAPS.md](./M0_SPEC_GAPS.md).

## 2. Separate three different guarantees

### 2.1 Snapshot construction and integrity

Snapshot construction verifies representation-level invariants:

- input is valid UTF-8 JSON;
- duplicate object keys and non-finite numbers are rejected;
- bytes are already RFC 8785/JCS canonical (the builder does not silently normalize supplied bytes);
- schema version is positive;
- SHA-256 is calculated over the exact canonical bytes;
- persisted snapshots are reconstructed only after JCS and hash verification.

The canonical bytes are the source of truth. JSONB is only a derived projection.

JCS proves canonical representation and integrity. It does **not** prove that the configuration is a valid executable object.

### 2.2 Semantic object validation

A trusted Object Validator checks the content of one exact candidate. It must not treat successful JCS validation as a semantic pass.

The surviving M0 Addendum §5 explicitly requires every newly published snapshot to contain `limits.max_tokens_per_invocation` as a positive JSON integer. Missing, zero, negative, fractional, or string values make the snapshot invalid for invocation. This is a confirmed semantic invariant and must be checked before publication.

The surviving artifacts do not provide a complete semantic schema/rule set for tool declarations, provider configuration, permissions, or security policies. Do not invent those rules or report these areas as fully validated.

**Fail-closed rule:** publication must not proceed when a required validation rule is unknown, unsupported, or not implemented for a configuration feature that the runtime is about to rely on. This does not authorize rejecting arbitrary JSON fields merely because they are unfamiliar; field/extension compatibility itself needs a documented schema rule. Until that rule exists, the validator may only claim the explicitly implemented and evidenced validation coverage. No "fully M0-valid" claim is permitted.

### 2.3 Publication authorization and Store atomicity

- Owner API may request publication; it cannot call a state-transition path that independently moves an object to `Validating` or `Published`.
- Only the trusted validation use case may start validation and produce a successful validation result.
- The result is bound to the full identity tuple: tenant, object, object version, snapshot. It additionally carries the snapshot content hash and immutable validator/ruleset version so evidence is tied to the exact bytes and rules used.
- A result for another tenant, object, version, or snapshot is invalid. Missing, failed, stale, or mismatched evidence fails closed.
- Validator/ruleset versions are immutable identifiers. Changing validation semantics requires a new version; evidence must never be reinterpreted under a newer ruleset.
- `publish_version()` remains one PostgreSQL transaction. It checks the trusted validation result and expected current published-version pointer while holding the object row lock; then it supersedes the old version (if any), publishes the new version, updates the pointer, appends the event, and commits.
- Validation evidence does not replace expected-pointer CAS, row lock, state checks, tenant scope, or DB constraints. A stale pointer still rejects publication even when validation succeeded.
- Owner HTTP handlers call runtime use cases only. They do not receive a generic Store handle or invoke `begin_validation()` / `publish_version()` directly.

The exact representation and durability of the validation result (opaque in-process capability versus persisted receipt) must be chosen with the validator execution model. In either form, callers cannot construct a successful result from request JSON, a client boolean, a tenant ID, or an untrusted DTO. If validation becomes asynchronous or can be resumed by another worker/process, the result must be persisted and verified by Store in the publication transaction.

## 3. Atomic creation boundary

The current `ObjectRepository::create_draft_version()` and `SnapshotRepository::insert()` each open and commit their own tenant transaction. The Owner API must not expose them as one logical operation while allowing a half-created version.

Required Store operation: create the draft version and its snapshot in one tenant-scoped PostgreSQL transaction. Both inserts commit or roll back together. Validate that the snapshot's tenant/object/version identity matches the draft version before issuing SQL. The object row and version row must also be checked under the same tenant scope.

No network or validator I/O belongs inside this transaction. Snapshot/JCS construction can happen before it; the trusted semantic validator runs after the immutable version+snapshot pair has committed and before publication.

## 4. Schema compatibility issue requiring a decision

The canonical M0 SQL defines `UNIQUE (tenant_id, object_id, content_hash)` on `snapshots`, while each snapshot row also stores `object_version_id` and has a composite FK to that exact version. Therefore, two versions of the same object cannot each receive a distinct snapshot row with identical canonical bytes/hash.

The M0 acceptance plan requires reuse of a snapshot for repeated invocations of the same configuration, but does not settle the separate case of creating a *new version* whose content is byte-identical to an earlier version. Do not silently work around this by attaching the earlier version's snapshot to a different version: that would violate the required tenant/object/version/snapshot binding. Before implementing the Owner create-version contract, decide whether identical-content version creation is rejected/deduplicated at the version level or whether the canonical SQL contract is to be revised in a separate qembed-sql change. Rust must not independently change the physical schema.

## 5. Required failure behaviour and tests

At minimum, tests must prove:

- invalid/noncanonical JSON never reaches persistence;
- valid JCS but semantically invalid snapshot cannot obtain publication authorization;
- the known positive-integer budget invariant is enforced;
- a validation result cannot be reused across tenant/object/version/snapshot identities;
- a result for a different snapshot hash or validator/ruleset version is rejected;
- validator/ruleset version changes do not retroactively alter the meaning of existing evidence;
- Owner API cannot invoke validation state transitions or mint successful evidence;
- failed snapshot insert rolls back draft-version insert, and failed version insert leaves no snapshot;
- validation failure leaves the object unpublished and does not move the published pointer;
- stale expected pointer rejects publication even with valid evidence;
- successful replacement preserves the existing Store lock/CAS and atomic event/pointer/state semantics;
- all paths retain tenant Scope/RLS isolation;
- API errors do not reveal raw validator diagnostics, secrets, SQL details, or internal evidence.

HTTP status/error mapping for semantic validation failures, evidence mismatch, and stale publication must be documented in `docs/ERROR_HANDLING.md` before handlers are finalized.

## 6. Implementation order

1. Treat the original M0 v2.2 specification as unavailable; use only the surviving artifacts as verified requirements and maintain the gap register.
2. Resolve identical-content new-version semantics against canonical qembed-sql; if a schema change is needed, make it in qembed-sql first.
3. Define the domain validator port and opaque success result, including identity/hash/immutable-ruleset binding and error taxonomy. Limit claims to implemented, evidenced rules.
4. Add one Store transaction for draft-version + snapshot creation.
5. Make publication require trusted validation evidence while preserving the existing expected-pointer CAS and transaction.
6. Implement Owner HTTP DTOs/handlers as thin adapters over runtime use cases.
7. Add PostgreSQL integration tests and update error-handling documentation.

No seventh crate is introduced. Responsibilities stay within the existing six-crate architecture.
