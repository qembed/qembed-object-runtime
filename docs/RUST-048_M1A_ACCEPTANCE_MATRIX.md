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
| T-02 | PASS — Rust CI | `object_isolation::same_tenant_objects_cannot_cross_reference_versions_or_snapshots` rejects same-tenant cross-Object published pointers and Snapshot provenance, and proves Object A cannot load Object B's draft version through `load_publication_candidate`. |
| T-04 | PASS — DB level | Canonical SQL `tests/20_invocations_memory_tools.sql` rejects forbidden User/ThirdParty provenance claims. The application-path verifier is explicitly M2, not claimed here. |
| T-06 | PASS | Rust `invocation_idempotency.rs` covers concurrency, payload mismatch, rollback and tenant/Object/effective-Caller namespace; `owner_invoke.rs` and walking-skeleton E2E cover HTTP replay. |
| T-09 | PASS — covered immutable rows | Rust `snapshot_repository.rs`, `transactional_outbox.rs` and `restricted_immutable_access.rs` verify snapshot/event immutability and runtime-role restrictions. Published-version immutability is also part of canonical SQL lifecycle/role checks. |
| T-11 | PASS — bounded M1a path | `walking_skeleton_e2e.rs` reconstructs a fresh pool/worker after teardown; `rust_047_failure_injection.rs` covers pre-commit rollback and durable Unknown after timeout/restart. This does not imply production process supervision or outbox dispatch. |
| T-12 | PASS — Rust CI (no log sink in this slice) | Walking-skeleton E2E uses a secret sentinel Owner token; asserts its digest (not plaintext) is persisted and the token is absent from accepted/inspect API responses, events, outbox payloads and persisted prompt source. The mock provider is keyed by the exact prompt, so a credential injected into the prompt fails the E2E. This API/runtime slice emits no tracing/log records and has no logging facade; there is currently no log emission path to leak the credential. |
| T-14 | PASS — provider contract | `qembed-llm/src/structured_output.rs` tests JsonOnly acceptance/rejection, None plain-text behavior, capability negotiation and rejection before provider completion; `qembed-domain/src/chat.rs` tests mode serialization/defaulting. |
| T-19 | PASS | Rust `snapshot_repository.rs` verifies exact canonical bytes/hash and rejects mutation; `atomic_draft_snapshot.rs` covers immutable content reuse and collision fail-closed behavior. |
| T-20 | PASS — SQL CI | Canonical SQL `tests/25_m1a_acceptance_gaps.sql` stores ToolVersion v1 in an immutable Snapshot, rebinds the live Object tool to v2, and asserts the old Snapshot still names v1 while the live binding names v2. An UPDATE/DELETE trigger now makes ToolVersion rows immutable; the regression asserts update rejection. M1a proves snapshot pinning; tool execution remains later-phase work. |
| T-21 | PASS — DB level | Canonical SQL `tests/20_invocations_memory_tools.sql` covers invalid source/provenance combinations. The application verifier remains M2. |
| T-27 | PASS — DB/RLS level | Canonical SQL `tests/24_caller_memory_rls.sql` covers missing Caller context, A/B episodic memory, embeddings, conflict edges, writes and transaction-local context cleanup. Retrieval-path coverage remains M2. |
| T-29 | PASS — Rust + SQL CI | Events receive DB-assigned per-aggregate `stream_version` from independent counter rows with a unique `(tenant, aggregate_type, aggregate_id, stream_version)` key. `event_stream_concurrency.rs` runs 32 concurrent appends on each of two streams under `qembed_runtime` and checks gapless independent sequences. A NOLOGIN `qembed_event_allocator` SECURITY DEFINER trigger owner alone can mutate counters; SQL/Rust tests reject caller-supplied versions and direct runtime counter updates. |
| T-30 | PASS — Rust CI | `begin_invocation::one_thousand_invocations_of_one_configuration_pin_the_same_snapshot` creates 1,000 distinct invocations and asserts all pin one Snapshot. `atomic_draft_snapshot.rs` separately proves 1,000 identical configuration creations deduplicate and a changed ToolVersion binding creates a new immutable Snapshot. |
| T-36 | PASS — SQL role checks | Canonical SQL role/RLS smoke covers platform kill-switch read-only access and dispatcher delivery-data boundaries. |
| T-37 | PASS | Rust `qembed-domain` compile-fail doctests are an explicit stable/MSRV CI step and cover non-forgeable `AllowedCommand` construction/traits. |
| T-41 | PASS — DB constraint | Canonical SQL `tests/19_composite_tenant_fks.sql` covers tenant-bound event/outbox references; Rust `tenant_composite_foreign_keys.rs` also rejects cross-tenant outbox/event references. |
| T-42 | PASS — DB constraint | Canonical SQL `tests/20_invocations_memory_tools.sql` includes the mismatched interaction-message/Invocation Caller claim rejection. |
| T-49 | PASS | Rust `tenant_queries.rs` verifies no tenant rows are visible without context and forged writes are denied; SQL smoke has the same fail-closed probe. |
| T-50 | PASS — SQL role check | Canonical SQL smoke verifies runtime cannot mutate the platform kill switch. |
| T-51 | PASS — SQL role check | Canonical SQL role/definer smoke verifies dispatcher delivery access without general message/Caller access. |
| T-53 | PASS — SQL CI | Clean-schema migration calls `assert_tenant_rls_classification()`: it checks an explicit positive allowlist of every tenant-owned table, rejects extra nullable/misclassified `tenant_id` tables and unclassified public tables, and validates the restricted-global/pre-auth exceptions. SQL test 25 proves migration guard failure for both a nullable tenant table and a missing expected tenant table. |
| T-54 | PASS — SQL role matrix evidence | Canonical SQL role/definer smoke checks runtime, dispatcher, admin, retention and definer grants, including a real runtime login's inability to assume the definer role. |
| T-55 | PASS — SQL CI | `policy_decisions` now has a composite key binding command, Caller, Snapshot, ToolVersion, binding version, operation and arguments hash. `tool_executions` references that exact claim tuple. SQL test 25 rejects mixed command, Caller, Object, Snapshot and ToolVersion claims; test 20 covers decision Caller mismatch and existing tenant FK probes cover tenant boundaries. |
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

All seven previously open M1a gaps now have focused implementation and passing regression coverage in Rust PR #71 and canonical SQL PR #19.

- Canonical SQL smoke: **359/359 assertions passed on PostgreSQL 15 and 359/359 on PostgreSQL 16**, both with pgvector 0.8.7, on SQL PR #19 HEAD `538ba933eec07d9ffab554a2c9673464f413fe11` (run [#44807264847](https://github.com/qembed/qembed-sql/actions/runs/44807264847)).
- Rust CI: stable workspace, integration and PostgreSQL tests passed on PostgreSQL 15.10 and 16.4; Rust 1.85 MSRV workspace and PostgreSQL tests passed on Rust PR #71 HEAD `500a053d7051ca5f21d05c81b8e8caca3b11f3af` (run [#44807132778](https://github.com/qembed/qembed-object-runtime/actions/runs/44807132778)).
- The PostgreSQL 15 run initially caught a stale test assertion hard-coded to version 16; the harness now accepts both supported majors, and the corrected matrix is green.

**RUST-048 is complete.** The matrix covers every applicable M1a acceptance ID and records the evidence. The M2 portions of T-04, T-21 and T-27 remain explicitly outside M1a. RUST-049 still owns release-grade artifact publication; this report does not claim full T-01…T-64 certification.
