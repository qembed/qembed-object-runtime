# M0 specification gaps affecting RUST-044

**Status:** open register. This document records what cannot be verified from the surviving project artifacts. It does not reconstruct the lost M0 v2.2 source.

## Source availability

- Original M0 v2.2 source specification: **unavailable/lost**.
- Surviving normative material: M0 v2.3.3 r3 Finalization Addendum, ADR RT-039…RT-048, acceptance T-01…T-64, and canonical SQL artifacts in qembed-sql.
- Consolidated PDF in qembed-sql: marked r1; not re-rendered after r3. The Markdown addendum is authoritative for changes it explicitly covers.
- Rule: a missing requirement is not presumed absent. New decisions must be recorded as new project requirements/ADRs, never attributed to the lost M0.

## Confirmed baseline relevant to publication

| Area | Verified rule | Evidence |
|---|---|---|
| Snapshot representation | UTF-8 JSON, duplicate-key/non-finite rejection, exact RFC 8785/JCS canonical bytes, positive schema version, SHA-256 over exact bytes | Existing Rust snapshot/JCS contract and surviving project artifacts |
| Snapshot persistence | Canonical bytes are source of truth; JSONB is a derived projection | Canonical qembed-sql and existing Rust Store contract |
| Invocation budget | Every newly published snapshot contains `limits.max_tokens_per_invocation` as a positive JSON integer; missing/zero/negative/fractional/string values invalid | M0 v2.3.3 r3 Finalization Addendum §5 |
| Publication concurrency | Expected-current-pointer CAS and atomic state/pointer/event update remain Store/DB responsibilities | Existing Store contract and surviving ADR/SQL artifacts |
| Tenant isolation | Tenant-scoped persistence and composite tenant ownership constraints remain mandatory | Canonical qembed-sql and existing Store contract |

## Open semantic specification gaps

| ID | Gap | Why it matters | Safe handling until resolved |
|---|---|---|---|
| GAP-044-01 | Complete object/configuration semantic schema from lost M0 v2.2 is unavailable | Cannot claim complete configuration validity or infer required/optional fields | Validate only confirmed rules; do not invent schema rules |
| GAP-044-02 | Tool declaration semantics and supported tool configuration are not fully established by surviving artifacts | Incorrect acceptance may enable unsupported execution; over-rejection may break compatibility | Do not claim tool configuration is semantically validated; block execution/publication paths that rely on an unvalidated feature |
| GAP-044-03 | Provider configuration semantics and compatibility rules are not fully established | Provider selection/capability assumptions could be wrong | No inferred provider schema; require explicit contract before relying on fields |
| GAP-044-04 | Permissions/security-policy configuration semantics are not fully established | Publication must not imply that authorization/security policy was validated | Keep authorization enforcement independent; do not mint validation success for unimplemented required checks |
| GAP-044-05 | Unknown-field/extension compatibility policy is not established for each schema version | A global policy could incorrectly apply one version's compatibility rules to another; blanket rejection or acceptance can both break compatibility or hide unsupported semantics | The Schema Registry must select an explicit unknown-field policy per `schema_version`. Do not infer v1 policy from the version number; v1 remains unspecified until explicitly defined |
| GAP-044-06 | Semantics for creating a distinct version with byte-identical content were not specified by surviving M0 artifacts | `UNIQUE (tenant_id, object_id, content_hash)` plus version-bound FK prevents separate identical snapshot rows; T-30 only requires invocation-level reuse and does not require a new version | **Resolved by new approved RUST-044 contract (2026-11-19):** exact-byte version-level deduplication returns `identical_content` with existing version/snapshot IDs; same hash with different bytes fails closed; different bytes create version+snapshot atomically. See [qembed-sql ADR RUST-044](https://github.com/qembed/qembed-sql/blob/master/ADR_RUST-044_Identical_Content_Version_Deduplication.md). No DDL change |

## Required decisions before claiming full RUST-044 publication validation

1. Define each supported object-configuration schema version and its supported feature set from recoverable evidence or an explicitly approved new specification.
2. Specify a Schema Registry entry for every supported `schema_version`, identifying its Schema Validator, checked fields/structures, per-version unknown-field/extension policy, and which constraints are confirmed versus `not_validated`.
3. Define the v1 schema and its unknown-field/extension policy explicitly before claiming structural validation for `schema_version: 1`. A positive `schema_version` value (including `1`) identifies a requested version; it does not prove that the corresponding full schema or policy has been defined.
4. Define tool/provider/permission/security-policy validation requirements before those features can be considered validated.
5. Extend the immutable validator/ruleset versioning policy as additional rules are approved. The first bounded implementation uses the immutable code identifier `m0-r3-bounded-budget-v1` and binds evidence to tenant, object, version, snapshot, content hash, and validator version. Persistence/revalidation policy remains open if validation becomes asynchronous or evidence must survive process boundaries.
6. Define the public error taxonomy for semantic rejection, unsupported configuration, stale evidence, and stale publication pointer.

## Semantic validation coverage reporting

**Approved implementation increment (2026-11-19):** the domain validator exposes an explicit, version-bound coverage report alongside its opaque validation evidence. The report identifies the confirmed invocation-budget invariant as validated and maps the still-open semantic areas to GAP-044-01…05 as not validated.

- `not_validated` means the validator does not have an established rule set for that area. It does not mean arbitrary fields in that area are automatically invalid.
- The report is informational and is not a publication capability. Publication continues to require the separate opaque evidence and Store-side identity/hash verification.
- A runtime path must fail closed before relying on a feature whose required semantics are not validated. This is not permission to blanket-reject unknown JSON fields.
- The coverage report does not resolve any specification gap or define new tool/provider/permission/security-policy semantics.
- Adding or changing semantic rules requires an explicitly approved source/decision and a new immutable validator version. Existing evidence is never reinterpreted under new rules.

## Accepted new project decisions

- **Versioned Schema Registry policy (2026-11-20):** unknown-field/extension handling is defined independently by each schema version, not by one global rule. For every supported version, the Schema Registry must identify the applicable Schema Validator, the fields/structures it checks, that version's unknown-field policy, and the constraints that are confirmed versus `not_validated`. The policy for v1 is not yet defined. Supplying `schema_version: 1` does not imply that a complete v1 schema or unknown-field policy exists. This is a new approved project clarification, not recovered M0 behavior.
- **RUST-044 identical-content version semantics (2026-11-19):** exact canonical-byte equality within the same tenant/object returns the existing version/snapshot as `identical_content`; hash match with byte mismatch is an integrity error; different bytes create version+snapshot atomically. This is a new approved project contract, not recovered M0 behavior. See the [canonical qembed-sql ADR](https://github.com/qembed/qembed-sql/blob/master/ADR_RUST-044_Identical_Content_Version_Deduplication.md).

## Decision hygiene

For every newly approved rule, record:
- decision ID and date;
- whether it is recovered from a surviving source or newly specified;
- exact source/section or explicit approval;
- validator/ruleset version in which it becomes effective;
- positive and negative test cases.

Until these gaps are closed, the implementation may provide a bounded walking skeleton, but must not claim complete M0 semantic validation or production-ready support for unverified object features.
