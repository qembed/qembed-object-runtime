# RUST-044 — Owner API and trusted publication validation

**Status:** accepted architectural constraints; implementation contract pending the M0 specification review and the schema questions listed below.

## 1. Scope and source hierarchy

RUST-044 is the M1a Owner API walking-skeleton slice: create object/version, publish, invoke, inspect invocation. The Owner API is a transport boundary, not a publication authority.

The normative source currently available in machine-readable form is the M0 v2.3.3 r3 Finalization Addendum, together with ADR RT-039…RT-048 and acceptance T-01…T-64. The addendum says the original M0 v2.2 specification retains unchanged domain/API sections, but the consolidated PDF in the SQL repository is explicitly marked as not re-rendered after r1. The Markdown addendum takes precedence for the changes it covers. The original specification's complete object-configuration validation section still needs a direct text-level review before claiming that M0 contains no further validation rules.

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

The M0 addendum explicitly requires every newly published snapshot to contain `limits.max_tokens_per_invocation` as a positive JSON integer. Missing, zero, negative, fractional, or string values make the snapshot invalid for invocation. This is a known semantic invariant and must be checked before publication.

The reviewed addendum, ADRs and acceptance plan do not define a complete schema/rule set for tool declarations, provider configuration, permissions, or security policies. Do not invent those rules or report them as validated. Before implementing broader semantic checks, review the original M0 v2.2 object/configuration sections. If those sections do not specify the rules, record a separate M0 specification gap and keep the validator fail-closed for unsupported/unknown configuration rather than pretending to validate it.

### 2.3 Publication authorization and Store atomicity

- Owner API may request publication; it cannot call a state-transition path that independently moves an object to `Validating` or `Published`.
- Only the trusted validation use case may start validation and produce a successful validation result.
- The result is bound to the full identity tuple: tenant, object, object version, snapshot. It should additionally carry the snapshot content hash and validator/ruleset version so the evidence is tied to the exact immutable bytes and the rules used.
- A result for another tenant, object, version, or snapshot is invalid. Missing, failed, stale, or mismatched evidence fails closed.
- `publish_version()` remains one PostgreSQL transaction. It checks the trusted validation result and the expected current published-version pointer while holding the object row lock; then it supersedes the old version (if any), publishes the new version, updates the pointer, appends the event, and commits.
- Validation evidence does not replace the expected-pointer CAS, row lock, state checks, tenant scope, or DB constraints. A stale pointer still rejects publication even when validation succeeded.
- Owner HTTP handlers call runtime use cases only. They do not receive a generic Store handle or invoke `begin_validation()` / `publish_version()` directly.

The exact representation and durability of the validation result (opaque in-process capability versus persisted receipt) must be chosen with the validator execution model. In either form, callers cannot construct a successful result from request JSON, a client boolean, a tenant ID, or an untrusted DTO. If validation becomes asynchronous or can be resumed by another worker/process, the result must be persisted and verified by Store in the publication transaction.

## 3. Atomic creation boundary

The current `ObjectRepository::create_draft_version()` and `SnapshotRepository::insert()` each open and commit their own tenant transaction. The Owner API must not expose them as one logical operation while allowing a half-created version.

Required Store operation: create the draft version and its snapshot in one tenant-scoped PostgreSQL transaction. Both inserts commit or roll back together. Validate that the snapshot's tenant/object/version identity matches the draft version before issuing SQL. The object row and version row must also be checked under the same tenant scope.

No network or validator I/O belongs inside this transaction. Snapshot/JCS construction can happen before it; the trusted semantic validator runs after the immutable version+snapshot pair has committed and before publication.

## 4. Schema compatibility issue requiring a decision

The canonical M0 SQL defines `UNIQUE (tenant_id, object_id, content_hash)` on `snapshots`, while each snapshot row also stores `object_version_id` and has a composite FK to that exact version. Therefore, two versions of the same object cannot each receive a distinct snapshot row with identical canonical bytes/hash.

The M0 acceptance plan requires reuse of a snapshot for repeated invocations of the same configuration, but does not settle the separate case of creating a *new version* whose content is byte-identical to an earlier version. Do not silently work around this by attaching the earlier version's snapshot to a different version: that would violate the requested tenant/object/version/snapshot binding. Before implementing the Owner create-version contract, decide whether identical-content version creation is rejected/deduplicated at the version level or whether the canonical SQL contract is to be revised in a separate qembed-sql change. Rust must not independently change the physical schema.

## 5. Required failure behaviour and tests

At minimum, tests must prove:

- invalid/noncanonical JSON never reaches persistence;
- valid JCS but semantically invalid snapshot cannot obtain publication authorization;
- the known positive-integer budget invariant is enforced;
- a validation result cannot be reused across tenant/object/version/snapshot identities;
- a result for a different snapshot hash or validator/ruleset version is rejected;
- Owner API cannot invoke validation state transitions or mint successful evidence;
- failed snapshot insert rolls back draft-version insert, and failed version insert leaves no snapshot;
- validation failure leaves the object unpublished and does not move the published pointer;
- stale expected pointer rejects publication even with valid evidence;
- successful replacement preserves the existing Store lock/CAS and atomic event/pointer/state semantics;
- all paths retain tenant Scope/RLS isolation.

HTTP status/error mapping for semantic validation failures, evidence mismatch, and stale publication must be documented in `docs/ERROR_HANDLING.md` before handlers are finalized. Responses must not expose raw validator diagnostics, secrets, SQL details, or internal evidence.

## 6. Implementation order

1. Complete the original M0 v2.2 specification review and record the exact semantic validation rules and explicit non-goals.
2. Resolve identical-content new-version semantics against canonical qembed-sql; if a schema change is needed, make it in qembed-sql first.
3. Define the domain validator port and opaque success result, including identity/hash/ruleset binding and error taxonomy.
4. Add one Store transaction for draft-version + snapshot creation.
5. Make publication require trusted validation evidence while preserving the existing expected-pointer CAS and transaction.
6. Implement Owner HTTP DTOs/handlers as thin adapters over runtime use cases.
7. Add PostgreSQL integration tests and update error-handling documentation.

No seventh crate is introduced. Responsibilities stay within the existing six-crate architecture.
