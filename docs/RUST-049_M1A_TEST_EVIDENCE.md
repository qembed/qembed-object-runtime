# RUST-049 — M1a test evidence artifacts

**Status: complete.**

The Rust stable CI matrix publishes one artifact per PostgreSQL major version. Each artifact is named `m1a-test-evidence-postgres-15`, `-16` or `-19` and is retained by GitHub Actions for 90 days. CI run [#44811246397](https://github.com/qembed/qembed-object-runtime/actions/runs/44811246397) passed on exact branch HEAD `1304b9c99096c7251c43edaca6623ab933d05db0`.

## Artifact contents

- `database-metadata.txt`: commit SHA, Rust compiler version, PostgreSQL server version and numeric version, and installed pgvector extension version.
- `ci/nextest-junit.xml`: JUnit report for the workspace unit and integration test binaries (Nextest writes it under `target/test-evidence/ci/`).
- `nextest.log`: complete nextest output, including test names and pass/fail results.
- `doc-tests.log`: workspace rustdoc/compile-fail test output, run separately because nextest does not execute doctests.
- `concurrency-results.txt`: focused log lines for the independent event-stream concurrency test and the 1,000-invocation / 1,000-snapshot-deduplication regression tests.

The test harness creates the pgvector extension before collecting metadata. The stable matrix uses pgvector 0.8.7 images for PostgreSQL 15, 16 and 19. The verified artifacts record PostgreSQL 15.20, 16.16 and 19.0 respectively; each reports 212/212 nextest tests passed, zero skipped and zero failures. The Rust 1.85 MSRV job uses the same test harness and database image default, but does not publish a separate artifact. Its workspace and PostgreSQL test job also passed in run #44811246397.

## Verified artifacts

- [PostgreSQL 15 artifact](https://github.com/qembed/qembed-object-runtime/actions/runs/44811246397/artifacts/14668130250)
- [PostgreSQL 16 artifact](https://github.com/qembed/qembed-object-runtime/actions/runs/44811246397/artifacts/14668050331)
- [PostgreSQL 19 artifact](https://github.com/qembed/qembed-object-runtime/actions/runs/44811246397/artifacts/14666147141)

All three JUnit reports contain 212 tests with zero failures/errors. The doctest logs include 32 passing domain compile-fail contracts and 7 passing store compile-fail contracts. The concurrency excerpt confirms the 1,000-identical-snapshot deduplication test, 1,000-invocation Snapshot pinning test, and independent event-stream concurrency tests passed.

The Nextest CI profile sets `test-threads = 1` because test binaries share one PostgreSQL database. Concurrency is still exercised inside the dedicated database tests themselves; this avoids unrelated integration binaries contending for shared fixtures.

## Reproduction

From a checkout with Docker Compose v2 and Rust installed:

```bash
docker compose --project-name qembed-rust-test --file compose.test.yaml up --detach --wait
bash scripts/test-postgres.sh
```

The script creates `target/test-evidence/`, captures DB metadata, runs nextest with JUnit output when cargo-nextest is installed, runs doctests, and writes concurrency-test excerpts. Without cargo-nextest it falls back to `cargo test --workspace`.

## Scope

Artifacts are evidence for the Rust workspace CI matrix, not a full T-01…T-64 certification. The canonical SQL smoke suite remains a separate workflow/repository and its results are linked from the RUST-048 acceptance matrix.
