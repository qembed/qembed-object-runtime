# RUST-046 — DB-backed provenance and Caller isolation evidence

## Result

RUST-046's M1a database-level scope is covered by the canonical SQL repository's direct PostgreSQL probes. The application-level provenance/retrieval verifier is **not** implemented or claimed here; it remains M2 work.

## Evidence sources

- Canonical SQL repository: [qembed/qembed-sql](https://github.com/qembed/qembed-sql)
- T-04/T-21 direct constraint probes: [tests/20_invocations_memory_tools.sql at validated SQL HEAD](https://github.com/qembed/qembed-sql/blob/bfa5d885ecd3ee96e59b57e4b2d17d172c532e6b/tests/20_invocations_memory_tools.sql)
- T-27 Caller-level RLS probes: [tests/24_caller_memory_rls.sql at validated SQL HEAD](https://github.com/qembed/qembed-sql/blob/bfa5d885ecd3ee96e59b57e4b2d17d172c532e6b/tests/24_caller_memory_rls.sql)
- Smoke runner: [tests/run_smoke.sh](https://github.com/qembed/qembed-sql/blob/bfa5d885ecd3ee96e59b57e4b2d17d172c532e6b/tests/run_smoke.sh)
- PostgreSQL workflow: [sql-smoke.yml](https://github.com/qembed/qembed-sql/blob/bfa5d885ecd3ee96e59b57e4b2d17d172c532e6b/.github/workflows/sql-smoke.yml)
- T-04/T-21 test addition: [SQL PR #17](https://github.com/qembed/qembed-sql/pull/17), merged as `2d5161eddcff26ea204163138375cd84e1afce8b`.
- T-27 Caller RLS remediation: [SQL PR #18](https://github.com/qembed/qembed-sql/pull/18), merged as `cf24f151220bea9007a51033905c0ea724d05280`. Validated source HEAD: `bfa5d885ecd3ee96e59b57e4b2d17d172c532e6b`.
- Successful PostgreSQL smoke run: [GitHub Actions run #44801382870](https://github.com/qembed/qembed-sql/actions/runs/44801382870), associated with validated SQL HEAD `bfa5d885ecd3ee96e59b57e4b2d17d172c532e6b`. The `postgres-smoke` job and `Run PostgreSQL smoke suite` step completed successfully.

The workflow provisions PostgreSQL 16 with `pgvector/pgvector:0.8.7-pg16-bookworm`. The runner applies the clean-install schema, executes both test files above, counts SQL-file errors and failed assertions, and exits non-zero if either count is non-zero. The successful run therefore validates the complete configured smoke suite at that SQL HEAD. It is a smoke suite, not a replacement for the full T-01..T-64 acceptance suite.

## Covered database invariants

### T-04 — provenance claims

Direct INSERT probes verify PostgreSQL rejects:
- `User` claiming `Verified`;
- `ThirdParty` claiming `Observed`;
- `RuntimeInference` claiming `Verified`.

### T-21 — source/verification combinations

Direct INSERT probes verify PostgreSQL rejects:
- `TrustedDocument` creating a `Verified` row without the required separate Owner-authored promotion row;
- `User` claiming `Observed`.

A positive control verifies a `Tool` / `Observed` row is accepted. These probes exercise database constraints/triggers directly, not only application validation.

### T-27 — Caller isolation

The dedicated PostgreSQL RLS test covers:
- semantic memories remaining tenant-scoped;
- episodic reads failing closed without Caller context;
- resolving a validated delegation to install transaction-local Caller context;
- Caller A and Caller B seeing only their own episodic memories;
- embeddings being visible only through visible parent memories;
- conflict edges being visible only when both endpoint memories are visible;
- rejection of cross-Caller inserts, embedding creation, conflict edges, and Caller reassignment;
- cross-Caller update/delete affecting no rows;
- Caller context being cleared at transaction commit.

The SQL PR documents the trust boundary: `app.caller_id` is a transaction-local setting installed by the trusted runtime after delegation validation. This is not protection against arbitrary SQL execution as the runtime database role.

## Explicit non-goals / remaining work

- No application-level retrieval/provenance verifier is implemented by RUST-046; that is M2.
- These SQL probes do not prove Rust-side retrieval ranking, source rendering, memory promotion workflow, or semantic correctness of retrieved content.
- The smoke run does not replace RUST-048's M1a acceptance report for all applicable acceptance IDs.
- The RUST-046 probes live in the companion canonical SQL repository because the Rust repository does not own the canonical `memories` schema or its DDL.

## Completion decision

RUST-046 is complete for its stated M1a DB-backed constraint scope: the negative and positive provenance probes and Caller-isolation RLS probes are included in the canonical SQL smoke runner, and that runner passed on PostgreSQL 16 / pgvector 0.8.7 at the cited commit. The M2 application verifier remains explicitly deferred.
