# RUST-074 — Application-level provenance and Caller isolation

## Application provenance boundary

The API application service accepts a structured claim document, not a caller-supplied evidence set. It loads the exact Knowledge chunks visible to the trusted Scope at the supplied pinned revision, then invokes the RUST-073 domain verifier with only those persisted chunks.

A client may propose citation IDs, but cannot provide or replace the evidence text used to resolve them. A citation that does not resolve inside the database retrieval result is rejected by V4. Revision visibility and tenant isolation remain enforced by the existing Knowledge retrieval Store.

This is deterministic provenance validation, not natural-language entailment. It does not claim that ordinary prose is semantically implied by a cited chunk.

## Episodic memory Caller boundary

Episodic memory reads and writes begin a signed Caller-scoped PostgreSQL transaction through `PgContext::begin_caller`. The requested Caller is resolved against the authenticated Principal's active direct delegation; the resulting `ResolvedCaller` is bound to the same transaction. The repository uses only that resolved Caller ID and also filters by it explicitly.

User-authored memory writes hard-code `source_kind = User` and `verification = Unverified`. The application API does not accept provenance assertions from the request. Other provenance transitions, including Owner promotion, remain separate workflows and are not implemented here.

PostgreSQL RLS remains an independent enforcement layer for episodic memories and dependent rows. Missing or revoked Caller delegation fails closed before the memory query.

## Acceptance coverage

- PostgreSQL integration: Caller A cannot read Caller B's episodic memory, even when both memories share tenant and Object.
- PostgreSQL integration: Caller A cannot select Caller B without a direct active delegation.
- PostgreSQL/API integration: a valid citation resolves only against persisted revision-visible evidence; a forged chunk ID is rejected.
- Unit tests: user claim bounds and control-character validation.

## Explicit non-goals

- No natural-language entailment model.
- No Owner memory promotion or Verified-row creation; that is RUST-075.
- No cross-Caller delegation or implicit Caller selection.
