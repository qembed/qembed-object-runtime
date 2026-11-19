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
| GAP-044-05 | Unknown-field/extension compatibility policy is not established | Blanket rejection could be incompatible; blanket acceptance could hide unsupported semantics | Do not reject arbitrary unknown fields by default or treat them as understood; define compatibility policy in a versioned schema decision |
| GAP-044-06 | Identical canonical content across two distinct versions conflicts with current snapshot uniqueness | `UNIQUE (tenant_id, object_id, content_hash)` plus version-bound FK prevents separate identical snapshot rows | Decide version-level reject/deduplication versus a separate qembed-sql revision; Rust must not bypass the constraint |

## Required decisions before claiming full RUST-044 publication validation

1. Define the versioned object-configuration schema and supported feature set from recoverable evidence or an explicitly approved new specification.
2. Define unknown-field and extension compatibility semantics.
3. Define tool/provider/permission/security-policy validation requirements before those features can be considered validated.
4. Resolve identical-content new-version semantics with qembed-sql owners.
5. Approve an immutable validator/ruleset versioning policy. Evidence must bind tenant, object, version, snapshot, content hash, and ruleset version.
6. Define the public error taxonomy for semantic rejection, unsupported configuration, stale evidence, and stale publication pointer.

## Decision hygiene

For every newly approved rule, record:
- decision ID and date;
- whether it is recovered from a surviving source or newly specified;
- exact source/section or explicit approval;
- validator/ruleset version in which it becomes effective;
- positive and negative test cases.

Until these gaps are closed, the implementation may provide a bounded walking skeleton, but must not claim complete M0 semantic validation or production-ready support for unverified object features.
