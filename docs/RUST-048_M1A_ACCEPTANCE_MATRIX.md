# RUST-048 — M1a acceptance coverage matrix

**Status: assessment in progress; RUST-048 is not complete.**

This report maps every M1a acceptance ID in the canonical `qembed/qembed-sql/acceptance_T-01_to_T-64.md` plan to available evidence. It deliberately distinguishes a passing test from partial coverage and from a test that has not yet been demonstrated. The SQL smoke suite is not treated as a substitute for the full T-01…T-64 acceptance run.

## Evidence runs and limits

- Rust CI run [#44802625195](https://github.com/qembed/qembed-object-runtime/actions/runs/44802625195) passed on PR #70 head `9a04e93d999830ce058aae86a7b7c9c4d7980984`. The workflow runs formatting, workspace check, Clippy, dependency/architecture checks, compile-fail doctests, SQLx offline validation and `scripts/test-postgres.sh`. This proves the Rust workspace test suite passed on that commit; it does not prove every acceptance ID below.
- Canonical SQL smoke run #44801382870 passed on SQL PR #18 head `bfa5d885ecd3ee96e59b57e4b2d17d172c532e6b`, PostgreSQL 16 + pgvector 0.8.7. It includes `tests/24_caller_memory_rls.sql`. The SQL harness exits non-zero for any SQL file error or failed assertion.
- The canonical `tests/run_smoke.sh` itself explicitly says it is **not** a replacement for T-01…T-64 CI. It recreates a DB using a superuser connection and uses `SET ROLE` for many checks; only selected runtime-login checks use a real login.
- PostgreSQL 15 compatibility, a full T-01…T-64 run, the complete failure-injection matrix and release-grade evidence artifacts are not established by these two runs. RUST-049 owns artifact publication.

## M1a acceptance IDs

| ID | Result | Evidence / remaining boundary |
|---|---|---|
| T-01 | PASS | Rust `tenant_queries::typed_queries_enforce_tenant_isolation_and_fail_closed_without_scope`; canonical SQL tenant RLS smoke. |
| T-02 | PARTIAL | Rust `tenant_composite_foreign_keys::composite_foreign_keys_reject_cross_tenant_references` and repository/API cross-tenant negatives cover tenant-bound references. A dedicated same-tenant Object A/Object B isolation matrix is not identified yet. |
| T-04 | PASS — DB level | Canonical SQL `tests/20_invocations_memory_tools.sql` rejects forbidden User/ThirdParty provenance claims. The application-path verifier is explicitly M2, not claimed here. |
| T-06 | PASS | Rust `invocation_idempotency.rs` covers concurrency, payload mismatch, rollback and tenant/Object/effective-Caller namespace; `owner_invoke.rs` and walking-skeleton E2E cover HTTP replay. |
| T-09 | PASS — covered immutable rows | Rust `snapshot_repository.rs`, `transactional_outbox.rs` and `restricted_immutable_access.rs` verify snapshot/event immutability and runtime-role restrictions. Published-version immutability is also part of canonical SQL lifecycle/role checks. |
| T-11 | PASS — bounded M1a path | `walking_skeleton_e2e.rs` reconstructs a fresh pool/worker after teardown; `rust_047_failure_injection.rs` covers pre-commit rollback and durable Unknown after timeout/restart. This does not imply production process supervision or outbox dispatch. |
| T-12 | PARTIAL | `event_outbox.rs` unit tests assert typed Object event payloads contain only allowlisted fields and no secret/token/raw-payload/arguments/credentials keys. A cross-surface capture test covering prompt, event, API response and logs is not identified. |
| T-14 | PASS — provider contract | `qembed-llm/src/structured_output.rs` tests JsonOnly acceptance/rejection, None plain-text behavior, capability negotiation and rejection before provider completion; `qembed-domain/src/chat.rs` tests mode serialization/defaulting. |
| T-19 | PASS | Rust `snapshot_repository.rs` verifies exact canonical bytes/hash and rejects mutation; `atomic_draft_snapshot.rs` covers immutable content reuse and collision fail-closed behavior. |
| T-20 | NOT YET EVIDENCED | No Rust ToolVersion snapshot-resolution test was identified. The current Rust `qembed-tools` crate has no execution implementation; do not treat this acceptance case as passed. |
| T-21 | PASS — DB level | Canonical SQL `tests/20_invocations_memory_tools.sql` covers invalid source/provenance combinations. The application verifier remains M2. |
| T-27 | PASS — DB/RLS level | Canonical SQL `tests/24_caller_memory_rls.sql` covers missing Caller context, A/B episodic memory, embeddings, conflict edges, writes and transaction-local context cleanup. Retrieval-path coverage remains M2. |
| T-29 | PARTIAL | SQL smoke includes stream-version uniqueness checks, but the acceptance plan explicitly requires concurrent independent invocation streams; the smoke evidence does not establish that concurrency property. |
| T-30 | PARTIAL | Rust `atomic_draft_snapshot.rs` covers identical-content reuse and concurrent identical writes. The stated 1,000-invocation reuse scale and binding-change/new-snapshot case are not demonstrated. |
| T-36 | PASS — SQL role checks | Canonical SQL role/RLS smoke covers platform kill-switch read-only access and dispatcher delivery-data boundaries. |
| T-37 | PASS | Rust `qembed-domain` compile-fail doctests are an explicit stable/MSRV CI step and cover non-forgeable `AllowedCommand` construction/traits. |
| T-41 | PASS — DB constraint | Canonical SQL `tests/19_composite_tenant_fks.sql` covers tenant-bound event/outbox references; Rust `tenant_composite_foreign_keys.rs` also rejects cross-tenant outbox/event references. |
| T-42 | PASS — DB constraint | Canonical SQL `tests/20_invocations_memory_tools.sql` includes the mismatched interaction-message/Invocation Caller claim rejection. |
| T-49 | PASS | Rust `tenant_queries.rs` verifies no tenant rows are visible without context and forged writes are denied; SQL smoke has the same fail-closed probe. |
| T-50 | PASS — SQL role check | Canonical SQL smoke verifies runtime cannot mutate the platform kill switch. |
| T-51 | PASS — SQL role check | Canonical SQL role/definer smoke verifies dispatcher delivery access without general message/Caller access. |
| T-53 | NOT YET EVIDENCED | No single full audit result was found proving every tenant-owned table has ENABLE + FORCE RLS and that migration aborts on every omitted/misclassified table. |
| T-54 | PASS — SQL role matrix evidence | Canonical SQL role/definer smoke checks runtime, dispatcher, admin, retention and definer grants, including a real runtime login's inability to assume the definer role. |
| T-55 | NOT YET EVIDENCED | Composite FK coverage exists for several relationships, but a complete assertion matrix for tenant/Object/Caller/Snapshot/Tool claims across invocation, decision, event and execution rows was not identified. |
| T-57 | PASS — SQL definer checks | Canonical SQL `tests/22_definer_security.sql` checks isolated non-login definer role, fixed search_path, revoked PUBLIC execution, membership and least-privilege grants. |
| T-60 | PASS — DB level | Canonical SQL `tests/20_invocations_memory_tools.sql` tests candidate-to-Verified update rejection, Owner promotion, TenantAdmin rejection and immutability. |
| T-61 | PASS — DB level | Canonical SQL role/retention smoke checks retention allowlist and append-only protections; R3-03 verifies terminal tool-output-only cleanup. |
| T-63 | PASS — Rust + DB parts | Rust JCS tests cover RFC 8785 number/key-order vectors, duplicate keys, non-finite numbers, invalid UTF-8 and noncanonical bytes; snapshot DB tests verify hash and JSONB projection guards. |

## R3 regression cases relevant to M1a

| ID | Result | Evidence / remaining boundary |
|---|---|---|
| R3-01 | PASS — DB level | Canonical SQL lifecycle smoke verifies publication pointer/state consistency and superseding behavior. |
| R3-02 | PASS — DB level | Canonical SQL provenance/memory smoke verifies deleting a superseded candidate preserves the promoted Verified row and blocks direct Verified mutation. |
| R3-03 | PASS — DB level | Canonical SQL retention smoke verifies only terminal tool-execution output fields are purged and runtime cannot invoke retention directly. |
| R3-06 | PASS — DB level | Canonical SQL role smoke verifies the admin role can create the required knowledge revision initialization row. |

## Current conclusion

RUST-048 is **not ready to close**. Passing Rust CI and the canonical SQL smoke run provide useful evidence, but the matrix still has explicit gaps/partial cases: **T-02, T-12, T-20, T-29, T-30, T-53 and T-55**. The M2 portions of T-04, T-21 and T-27 are intentionally outside M1a and must not be pulled forward as M1a requirements.

Next work should add focused regression coverage for these M1a gaps (or document an explicit scope decision where the acceptance plan's phase assignment is stale), then rerun the applicable Rust/PostgreSQL and SQL acceptance jobs. Do not mark RUST-048 or M1a complete until those gaps are resolved and the report is updated with exact run evidence.
