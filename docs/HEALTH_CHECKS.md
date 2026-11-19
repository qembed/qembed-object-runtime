# Health and readiness contract

The API exposes two unauthenticated operational endpoints:

- `GET /health/live` — process liveness only. It returns HTTP 200 with
  `{"status":"ok"}` and does not query PostgreSQL. A database outage must not
  cause an orchestrator to restart an otherwise healthy process.
- `GET /health/ready` — service readiness. It returns HTTP 200 with
  `{"status":"ready"}` only when PostgreSQL answers and the database migration
  history exactly matches the migrations bundled with this binary. Otherwise
  it returns HTTP 503 with the fixed body `{"status":"not_ready"}`.

## Readiness checks

The store adapter performs read-only checks in this order:

1. Execute `SELECT 1` against the configured PostgreSQL pool.
2. Load the migration set bundled with this binary.
3. Read `_sqlx_migrations` and compare the complete ordered version/checksum
   set with the bundled migrations. Every applied row must have
   `success = true`.

A missing migration table, pending migration, unknown extra migration, failed
migration row, or checksum mismatch makes the service not ready. A connection
or query failure is classified as database unavailable. The probe never runs
or repairs migrations; schema changes remain an explicit deployment operation.

## Public response and security

The readiness endpoint intentionally collapses dependency failure and migration
mismatch into the same public 503 response. It does not serialize SQLx errors,
database hostnames, connection strings, migration versions, checksums, SQL, or
credentials. The detailed failure category remains an internal typed value.

These endpoints are operational signals, not authentication or tenant-scoped
business APIs. They do not accept tenant identifiers and do not open
tenant-scoped transactions.

## Test coverage

- Actix API integration tests inject ready, database-unavailable, and
  migration-mismatch probe results to verify HTTP status/body behavior,
  liveness independence, and response minimization.
- PostgreSQL store integration tests verify a real migrated database is ready
  and a closed pool is unavailable.
- Store unit tests verify exact migration version/checksum/success matching,
  including pending, extra, failed, and checksum-mismatched rows.
