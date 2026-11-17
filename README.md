# Qembed Object Runtime — Rust Starter

This is an intentionally minimal Rust workspace scaffold for implementing
`ROADMAP_RUST_v1.md`.

## Workspace crates

- `qembed-domain`
- `qembed-store`
- `qembed-llm`
- `qembed-tools`
- `qembed-runtime` (also hosts the `qembed-eval` binary target)
- `qembed-api`

No runtime behavior or dependencies have been implemented yet.

## Build and test

```bash
cargo check --workspace
cargo test --workspace
```

## PostgreSQL integration tests

Docker Engine and Docker Compose v2 are required. The test runner starts the
pinned PostgreSQL 16.4 service, waits for its health check, runs the complete
workspace test suite, and removes the container and its test volume on exit.

Run locally:

```bash
bash scripts/test-postgres.sh
```

To use a different connection string, set `DATABASE_URL` before running the
script. The integration-test fixture is shared under
`crates/qembed-store/tests/common`.

## Git

The repository is initialized and contains an initial commit
(`chore: initialize Rust workspace and roadmap`).
