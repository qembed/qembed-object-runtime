# M1a release notes — Rust Object Runtime

**Release gate:** RUST-050  
**Date:** 2026-11-21  
**Status:** release candidate; completion is conditional on the full CI run for the RUST-050 release commit.

## Delivered

M1a establishes the bounded Rust walking skeleton:

`create → publish → invoke → retrieve result`

The flow is exercised through the Owner HTTP API, PostgreSQL persistence, the Runtime worker and a deterministic mock ChatProvider. The milestone is about a durable, tenant-isolated invocation path; it is not a production-ready general-purpose worker or external tool execution platform.

## Acceptance and evidence

| Task | Result | Evidence |
|---|---|---|
| RUST-045 — walking skeleton | Complete | [E2E report](RUST-045_WALKING_SKELETON_E2E.md); real PostgreSQL, Owner HTTP flow, idempotent admission, teardown/reconnect, fresh worker, result retrieval and cross-tenant 404. |
| RUST-046 — DB provenance constraints | Complete | Canonical SQL provenance checks reject forbidden provenance claims. The application-level verifier remains M2. |
| RUST-047 — failure injection | Complete | [Failure-injection report](RUST-047_FAILURE_INJECTION.md); pre-COMMIT backend termination rolls back the write set; committed work survives teardown; ambiguous provider timeout remains OutcomeUnknown and is not automatically retried after restart. |
| RUST-048 — acceptance suite | Complete | [M1a acceptance matrix](RUST-048_M1A_ACCEPTANCE_MATRIX.md) records each applicable M1a acceptance ID and its evidence/remaining boundary. |
| RUST-049 — reproducible evidence | Complete | [Evidence artifact contract](RUST-049_M1A_TEST_EVIDENCE.md); stable CI artifacts for PostgreSQL 15/16/19, JUnit/nextest, database metadata, full logs, doctests and concurrency excerpts. |

### Verified CI evidence preceding this release candidate

- Rust acceptance run [#44809515677](https://github.com/qembed/qembed-object-runtime/actions/runs/44809515677): stable workspace and PostgreSQL integration matrix on PostgreSQL 15/16/19, plus Rust 1.85 MSRV.
- Canonical SQL run [#44808543381](https://github.com/qembed/qembed-sql/actions/runs/44808543381): 359/359 assertions on PostgreSQL 15/16/19 with pgvector 0.8.7.
- Full Rust evidence run [#44811246397](https://github.com/qembed/qembed-object-runtime/actions/runs/44811246397): 212/212 nextest tests on PostgreSQL 15.20, 16.16 and 19.0 with pgvector 0.8.7; Rust 1.85 MSRV passed.
- RUST-049 PR-head run [#44811741674](https://github.com/qembed/qembed-object-runtime/actions/runs/44811741674): stable PostgreSQL 15/16/19 matrix and Rust 1.85 MSRV passed on the exact PR HEAD; fresh evidence artifacts were uploaded for all three PostgreSQL versions.

The RUST-050 release candidate itself must pass the complete repository CI workflow (including formatting, check, Clippy, dependency/architecture checks, SQLx offline validation, unit/integration/PostgreSQL tests and Rust 1.85 MSRV). Its exact commit and workflow run are recorded in the RUST-050 pull request.

## Known limitations — explicitly outside M1a

1. **No full T-01…T-64 certification.** The acceptance matrix covers only applicable M1a IDs. It does not claim completion of all acceptance IDs.
2. **Application provenance verification and retrieval are M2.** In particular, the application-path verifier portions of T-04/T-21 and retrieval-path Caller isolation in T-27 are not claimed as M1a deliverables.
3. **No production outbox dispatcher/redelivery or process supervisor.** The E2E test restarts with a fresh pool/worker and directly supplies the persisted Invocation ID; it does not prove dispatcher-driven pickup or production process supervision.
4. **Ambiguous external provider outcomes are not reconciled.** M1a proves fail-closed OutcomeUnknown behavior with a deterministic mock. Provider-side idempotency and external outcome reconciliation remain future work.
5. **No external provider or write-tool execution.** The walking skeleton uses MockChatProvider. Tool authorization, SSRF-safe networking, execution and reconciliation belong to M3.
6. **Caller and budget provisioning are fixtures in the E2E.** Explicit Caller delegation and Object budget are provisioned by test setup, not by the Owner API.
7. **No logging sink in this runtime slice.** The secret-hygiene test covers API responses, persisted rows, events, outbox and provider prompt; this slice has no tracing/log emission path to inspect.
8. **Canonical SQL clean-schema scope.** The SQL evidence is for the canonical clean-schema test/install path; it is not an in-place upgrade migration for an already deployed database.
9. **Transport and operational scope.** This milestone does not deliver Connect/gRPC, production deployment, SRE operations or compliance sign-off.

## Defect review

At the RUST-050 review, the repository had no open GitHub issues and no open pull requests. This records the state of this repository's GitHub issue/PR tracker; it is not a claim that every external or product-wide defect tracker has been audited.

## Completion criterion

M1a is complete only when the RUST-050 release candidate's full CI run is green on its exact commit and the resulting evidence remains attached to the pull request. RUST-050 does not authorize starting M1b/M2/M3 work to be treated as part of M1a, nor does it relax the limitations above.
