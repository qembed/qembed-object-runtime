# Qembed Object Runtime — Rust workspace

This repository contains the Rust workspace for implementing
[`ROADMAP_RUST_v1.md`](ROADMAP_RUST_v1.md). The project is intentionally being
built incrementally; not all runtime behavior is implemented yet.

## Workspace crates

- `qembed-domain` — domain contracts and types
- `qembed-store` — PostgreSQL persistence and explicit SQLx migrations
- `qembed-llm` — chat-provider adapters
- `qembed-tools` — tool adapters and execution
- `qembed-runtime` — orchestration; also contains the `qembed-eval` binary
- `qembed-api` — API and application composition root

Do not add a seventh crate or move `qembed-eval` into a separate crate without
an architecture decision.

## Prerequisites

- Rust stable toolchain with the `rustfmt` and `clippy` components
- Rust 1.85.0 (the workspace MSRV; needed to reproduce the MSRV CI job)
- Docker Engine and Docker Compose v2 for PostgreSQL integration tests
- Python 3 for the crate-boundary check
- SQLx CLI 0.8.6 for the SQLx metadata check (installation below)

Install the Rust toolchains:

```bash
rustup toolchain install stable --component rustfmt --component clippy
rustup toolchain install 1.85.0
```

## Quick start

From the repository root:

```bash
cargo check --workspace --all-targets
cargo test --workspace --lib
bash scripts/test-postgres.sh
```

The first command checks every workspace target. The second runs library unit
tests without the PostgreSQL integration test. The test script runs the full
workspace test suite, including integration tests, against PostgreSQL 16.4.

## Standard validation commands

These are the main checks used by CI:

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo tree -d
python3 scripts/check_architecture.py
bash scripts/test-postgres.sh
```

`cargo tree -d` reports duplicate dependency versions for review; it is
informational rather than a pass/fail policy by itself.

To check the declared minimum supported Rust version:

```bash
cargo +1.85.0 check --workspace
RUSTUP_TOOLCHAIN=1.85.0 bash scripts/test-postgres.sh
```

The environment override makes every `cargo` invocation inside the test
script use Rust 1.85.0 as well.

## PostgreSQL integration tests

The integration-test harness uses the pinned `postgres:16.4-alpine` image in
[`compose.test.yaml`](compose.test.yaml). The runner:

1. Starts the PostgreSQL service and waits for its health check.
2. Sets `DATABASE_URL` to the disposable test database unless it is already set.
3. Runs `cargo test --workspace`.
4. Removes the test container, network, and volume on exit.

Run the full suite with:

```bash
bash scripts/test-postgres.sh
```

The default connection string is:

```text
postgres://qembed_test:qembed_test@127.0.0.1:55432/qembed_test
```

If overriding `DATABASE_URL`, it must point to a disposable, empty PostgreSQL
16 database: the integration test verifies the initial migration state and
applies migrations. Do not point the test harness at a database containing
valuable data. The shared SQLx fixture lives in
`crates/qembed-store/tests/common`.

## SQLx offline metadata

The CI metadata check uses SQLx CLI 0.8.6 and a running PostgreSQL test service.
Install the matching CLI:

```bash
cargo install sqlx-cli --version 0.8.6 --no-default-features --features rustls,postgres --locked
```

Then run the metadata and offline compilation checks:

```bash
docker compose --project-name qembed-rust-test --file compose.test.yaml up --detach --wait
DATABASE_URL=postgres://qembed_test:qembed_test@127.0.0.1:55432/qembed_test cargo sqlx prepare --check --workspace
SQLX_OFFLINE=true cargo check --workspace --all-targets
docker compose --project-name qembed-rust-test --file compose.test.yaml down --volumes --remove-orphans
```

The `.sqlx` directory is committed to the repository. There are currently no
SQLx query macros, so no query metadata JSON files are expected yet. When
compile-time checked SQL queries are introduced, regenerate metadata against
the development database with `cargo sqlx prepare` and commit the generated
files. The offline compile check uses that metadata; it does not inspect live
database rows or replace PostgreSQL integration tests.

## Run database migrations explicitly

Creating a pool does not apply migrations. To run the migration command against
the local test service:

```bash
docker compose --project-name qembed-rust-test --file compose.test.yaml up --detach --wait
DATABASE_URL=postgres://qembed_test:qembed_test@127.0.0.1:55432/qembed_test cargo run -p qembed-store --bin qembed-migrate
docker compose --project-name qembed-rust-test --file compose.test.yaml down --volumes --remove-orphans
```

The migration runner is deliberately separate from normal application startup.


## Invocation identity and atomic begin

Authenticated Principal identity is represented separately from the effective invocation Caller. Explicit Caller selections are validated through SQL-011 delegation in the same transaction that reserves idempotency, pins snapshot and knowledge revision, inserts the invocation, and appends its event/outbox message. Invocation pool is a separate explicit policy decision. See [Principal/Caller delegation](docs/PRINCIPAL_CALLER_DELEGATION.md) and [atomic begin-invocation](docs/BEGIN_INVOCATION_TRANSACTION.md).


## Invocation worker lifecycle

`begin_invocation` stores the original user message in the same transaction as the idempotency reservation and invocation. The worker resumes from the persisted invocation checkpoint, persists the assistant response before terminal completion, and can finish after a process restart without repeating a provider call once the response is in `Verifying`. Invocation leases, heartbeat, expiry/reclaim, and fencing-token checks are implemented (RUST-038); provider retries reuse a stable idempotency key derived from the durable invocation ID (RUST-038A), with upstream deduplication only when the configured adapter and provider guarantee it. Actual outbox dispatch remains a later task. See [invocation worker lifecycle](docs/INVOCATION_WORKER_LIFECYCLE.md), [lease and fencing contract](docs/INVOCATION_LEASES_AND_FENCING.md), and [ChatProvider contract](docs/CHAT_PROVIDER_CONTRACT.md).


## Error handling contract

Runtime failures use a provider-neutral typed taxonomy; the API owns stable
public error codes and HTTP status mapping. See [Error handling contract](docs/ERROR_HANDLING.md)
for classification rules, tenant-concealment behavior, sensitive-data boundaries,
and the prohibition on treating HTTP errors as permission to retry ambiguous
provider calls.


## Health and readiness

The API exposes process liveness and dependency readiness checks. Readiness verifies PostgreSQL connectivity and exact SQLx migration-history agreement without applying migrations. See [health checks](docs/HEALTH_CHECKS.md) for endpoint and response contracts.
