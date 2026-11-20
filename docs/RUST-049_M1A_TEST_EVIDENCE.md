# RUST-049 — M1a test evidence artifacts

**Status: implementation awaiting CI validation.**

The Rust stable CI matrix publishes one artifact per PostgreSQL major version. Each artifact is named `m1a-test-evidence-postgres-15`, `-16` or `-19` and is retained by GitHub Actions for 90 days.

## Artifact contents

- `database-metadata.txt`: commit SHA, Rust compiler version, PostgreSQL server version and numeric version, and installed pgvector extension version.
- `ci/nextest-junit.xml`: JUnit report for the workspace unit and integration test binaries (Nextest writes it under `target/test-evidence/ci/`).
- `nextest.log`: complete nextest output, including test names and pass/fail results.
- `doc-tests.log`: workspace rustdoc/compile-fail test output, run separately because nextest does not execute doctests.
- `concurrency-results.txt`: focused log lines for the independent event-stream concurrency test and the 1,000-invocation / 1,000-snapshot-deduplication regression tests.

The test harness creates the pgvector extension before collecting metadata. The stable matrix uses pgvector 0.8.7 images for PostgreSQL 15, 16 and 19. The Rust 1.85 MSRV job uses the same test harness and database image default, but does not publish a separate artifact.

## Reproduction

From a checkout with Docker Compose v2 and Rust installed:

```bash
docker compose --project-name qembed-rust-test --file compose.test.yaml up --detach --wait
bash scripts/test-postgres.sh
```

The script creates `target/test-evidence/`, captures DB metadata, runs nextest with JUnit output when cargo-nextest is installed, runs doctests, and writes concurrency-test excerpts. Without cargo-nextest it falls back to `cargo test --workspace`.

## Scope

Artifacts are evidence for the Rust workspace CI matrix, not a full T-01…T-64 certification. The canonical SQL smoke suite remains a separate workflow/repository and its results are linked from the RUST-048 acceptance matrix.
