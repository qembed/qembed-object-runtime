# Semantic validation coverage report

**Status:** implemented as an informational domain result for RUST-044 (2026-11-19).

## Purpose

JCS validation proves that snapshot bytes are valid, canonical, and hash-consistent. It does not prove that the configuration is semantically valid. The bounded semantic validator currently enforces only the confirmed M0 r3 positive-integer invariant for `limits.max_tokens_per_invocation`.

The coverage report makes that limited scope explicit so that a successful check of one rule cannot be mistaken for complete object validation.

## Domain contract

- `ObjectValidator::validate_with_report(snapshot)` returns a `ValidationOutcome`.
- `ValidationOutcome` contains:
  - opaque `ValidationEvidence`, bound to tenant, object, object version, snapshot, content hash, and immutable validator version;
  - informational `ValidationReport`, containing the validator version and a static list of coverage entries.
- `ObjectValidator::validate(snapshot)` remains available for existing trusted callers that need evidence only.
- Evidence remains non-serializable and cannot be constructed from API DTOs.
- The report is not evidence, an authorization capability, or a publication decision. Store publication continues to require and verify the separate evidence.
- The report is metadata about the implemented rules. It does not itself make a configuration valid or invalid.

## Current coverage

| Area | Status | Rule / gap | Meaning |
|---|---|---|---|
| Invocation budget | `validated` | `M0-R3-ADDENDUM-5` | `limits.max_tokens_per_invocation` must be a positive JSON integer |
| Complete object schema | `not_validated` | `GAP-044-01` | Complete semantic schema is unavailable |
| Tool configuration | `not_validated` | `GAP-044-02` | Tool declaration semantics are not established |
| Provider configuration | `not_validated` | `GAP-044-03` | Provider configuration semantics are not established |
| Permissions | `not_validated` | `GAP-044-04` | Permission semantics are not established |
| Security policies | `not_validated` | `GAP-044-04` | Security-policy semantics are not established |
| Unknown fields/extensions | `not_validated` | `GAP-044-05` | Compatibility policy is not established |

`not_validated` means no verified rule set is implemented for that area. It does **not** mean that arbitrary JSON fields in that area are automatically rejected. Conversely, the runtime must fail closed before relying on a feature whose required semantics are not validated. Authorization enforcement remains independent of this report.

## Versioning policy

The current identifier is `m0-r3-bounded-budget-v1`. This increment adds coverage reporting but does not change the semantic rule set, so the identifier remains unchanged.

Any change to semantic rules requires a new immutable validator version. Previously issued evidence must never be reinterpreted under a later version. Every newly approved rule must record its source or explicit project approval, gap/decision identifier, validator version, and positive/negative tests.

## Explicit non-goals

This report does not:
- reconstruct the lost M0 v2.2 specification;
- define new schema, tool, provider, permission, security-policy, or extension semantics;
- blanket-reject unknown fields;
- claim complete M0 semantic validation;
- replace execution-time authorization or policy checks.
