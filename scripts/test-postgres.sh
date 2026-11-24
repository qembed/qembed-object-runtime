#!/usr/bin/env bash
set -euo pipefail

if ! docker compose version >/dev/null 2>&1; then
  echo "Docker Compose v2 is required to run PostgreSQL integration tests." >&2
  exit 1
fi

compose=(docker compose --project-name qembed-rust-test --file compose.test.yaml)
export DATABASE_URL="${DATABASE_URL:-postgres://qembed_test:qembed_test@127.0.0.1:55432/qembed_test}"
export CONTROL_PLANE_DATABASE_URL="${CONTROL_PLANE_DATABASE_URL:-postgres://qembed_test:qembed_test@127.0.0.1:55433/qembed_control_plane_test}"
export NATS_URL="${NATS_URL:-nats://127.0.0.1:4222}"
export VALKEY_URL="${VALKEY_URL:-redis://127.0.0.1:56379/}"
evidence_dir="${TEST_EVIDENCE_DIR:-target/test-evidence}"
# Pin the canonical SQL revision that includes deterministic reservation expiry.
# Update only when the Rust/SQL cross-repository contract advances.
canonical_sql_commit="573a54a56797e713131e74c4830b3bcb6181abaf"
sql_repo_dir="${TMPDIR:-/tmp}/qembed-sql-rust-062-${canonical_sql_commit}"

mkdir -p "$evidence_dir"

psql_in_test_db() {
  "${compose[@]}" exec -T postgres psql -U qembed_test -d qembed_test -v ON_ERROR_STOP=1 "$@"
}

install_legacy_rt050_test_fixture() {
  local sql_commit="c78348b29a3d78e1a7ac77be6986b447387d6bc7"
  local sql_repo_dir="${TMPDIR:-/tmp}/qembed-sql-rt050-${sql_commit}"

  if [[ ! -d "$sql_repo_dir/.git" ]]; then
    rm -rf "$sql_repo_dir"
    git clone --no-checkout https://github.com/qembed/qembed-sql.git "$sql_repo_dir"
    git -C "$sql_repo_dir" fetch --depth 1 origin "$sql_commit"
    git -C "$sql_repo_dir" checkout --detach "$sql_commit"
  fi

  "${compose[@]}" exec -T --user root postgres bash -euxc '
    apt-get update
    apt-get install -y --no-install-recommends build-essential pkg-config libsodium-dev postgresql-server-dev-19
  '

  "${compose[@]}" cp "$sql_repo_dir/extensions/qembed_context_verify" postgres:/tmp/qembed_context_verify
  "${compose[@]}" exec -T --user root postgres bash -euxc '
    make -C /tmp/qembed_context_verify PG_CONFIG="$(command -v pg_config)"
    make -C /tmp/qembed_context_verify PG_CONFIG="$(command -v pg_config)" install
  '
  "${compose[@]}" cp "$sql_repo_dir/tests/qembed_context_verify_transaction_setup.sql" postgres:/tmp/qembed_context_verify_transaction_setup.sql
  psql_in_test_db -f /tmp/qembed_context_verify_transaction_setup.sql
}

write_concurrency_results() {
  local log_file=""
  if [[ -f "$evidence_dir/nextest.log" ]]; then
    log_file="$evidence_dir/nextest.log"
  elif [[ -f "$evidence_dir/cargo-test.log" ]]; then
    log_file="$evidence_dir/cargo-test.log"
  fi

  if [[ -n "$log_file" ]]; then
    grep -E 'event_stream_concurrency|one_thousand_invocations_of_one_configuration_pin_the_same_snapshot|one_thousand_identical_configuration_creations_reuse_one_snapshot' "$log_file" \
      > "$evidence_dir/concurrency-results.txt" || true
  fi
}

cleanup() {
  write_concurrency_results
  "${compose[@]}" down --volumes --remove-orphans
}
trap cleanup EXIT

"${compose[@]}" up --detach --wait

# The existing Rust SQLx integration suite still uses its isolated RT-050 verifier fixture.
install_legacy_rt050_test_fixture

# Run the normal Rust/PostgreSQL workspace suite against the Rust-owned test
# schema first. The QSA-003 canonical-schema test is deliberately isolated
# because the SQL clean-install schema is not interchangeable with SQLx
# development migrations.
if cargo nextest --version >/dev/null 2>&1; then
  cargo nextest run --workspace --profile ci 2>&1 | tee "$evidence_dir/nextest.log"
  cargo test --workspace --doc 2>&1 | tee "$evidence_dir/doc-tests.log"
else
  cargo test --workspace 2>&1 | tee "$evidence_dir/cargo-test.log"
fi

# RUST-064: serial, destructive JetStream restart tests. The loss test removes
# only the NATS container; its PostgreSQL recovery source remains untouched.
cargo test -p qembed-api --test rust_051c_nats_reconnect -- --ignored --test-threads=1 2>&1 \
  | tee "$evidence_dir/nats-reconnect.log"

# Reset PostgreSQL to a clean database before applying the canonical SQL
# candidate. This keeps the production-schema contract test isolated from the
# Rust repository's own SQLx migration fixture.
"${compose[@]}" down --volumes --remove-orphans
"${compose[@]}" up --detach --wait

if [[ ! -d "$sql_repo_dir/.git" ]]; then
  rm -rf "$sql_repo_dir"
  git clone --no-checkout https://github.com/qembed/qembed-sql.git "$sql_repo_dir"
  git -C "$sql_repo_dir" fetch --depth 1 origin "$canonical_sql_commit"
  git -C "$sql_repo_dir" checkout --detach "$canonical_sql_commit"
fi

"${compose[@]}" exec -T --user root postgres bash -euxc '
  apt-get update
  apt-get install -y --no-install-recommends build-essential pkg-config libsodium-dev postgresql-server-dev-19
'

"${compose[@]}" cp "$sql_repo_dir/extensions/qembed_context_verify" postgres:/tmp/qembed_context_verify
"${compose[@]}" exec -T --user root postgres bash -euxc '
  make -C /tmp/qembed_context_verify PG_CONFIG="$(command -v pg_config)"
  ldd /tmp/qembed_context_verify/qembed_context_verify.so | grep "libsodium.so.23"
  make -C /tmp/qembed_context_verify PG_CONFIG="$(command -v pg_config)" install
'

# Apply the canonical fresh-install schema, verifier/key registry, deterministic
# fixtures, qctx1/qctx2 verifier and Object-scoped Anonymous Caller migrations.
for sql_file in \
  qembed_m0_v2_3_3_clean_schema.sql \
  tests/qembed_context_verify_transaction_setup.sql \
  tests/qsa003_signed_context_seed.sql \
  migrations/rt050_signed_context_rls.sql \
  migrations/rt059_anonymous_context_v2.sql \
  migrations/rt060_anonymous_object_caller.sql \
  migrations/rt061_runtime_caller_erasure.sql \
  migrations/rt061b_verified_caller_erasure.sql \
  migrations/rt061c_cancelled_tool_execution_erasure.sql; do
  "${compose[@]}" cp "$sql_repo_dir/$sql_file" "postgres:/tmp/$(basename "$sql_file")"
  psql_in_test_db -f "/tmp/$(basename "$sql_file")"
done

{
  echo "rust_commit_sha=${GITHUB_SHA:-local}"
  echo "canonical_sql_commit=$canonical_sql_commit"
  echo "rustc_version=$(rustc --version)"
  echo "postgres_version=$(psql_in_test_db -Atc 'SHOW server_version')"
  echo "postgres_version_num=$(psql_in_test_db -Atc 'SHOW server_version_num')"
  echo "pgvector_version=$(psql_in_test_db -Atc "SELECT extversion FROM pg_extension WHERE extname = 'vector'")"
} > "$evidence_dir/qsa003-canonical-database-metadata.txt"

# These are real Rust ContextIssuer tests, not an independent Python probe:
# Rust creates/signs qctx1 and qctx2 tokens; PostgreSQL verifies them through
# the C/libsodium extension and canonical RLS/Anonymous Caller policies.
cargo test -p qembed-store --test context_issuer_postgres -- --ignored --test-threads=1 2>&1 \
  | tee "$evidence_dir/qsa003-rust-contextissuer-canonical.log"

# RUST-061 uses the same canonical Runtime schema plus the mandatory external
# Control-plane database. The test proves revoke -> external tombstone -> erase
# ordering and repeated deletion idempotency through the Rust Store adapter.
cargo test -p qembed-store --test caller_deletion_postgres -- --ignored --test-threads=1 2>&1 \
  | tee "$evidence_dir/rust-061-caller-deletion-canonical.log"

# RUST-062 uses the canonical worker-only reservation expiry function. The test
# runs competing qembed_worker sessions against one safe and one ambiguous
# reservation, proving the counters are released exactly once.
cargo test -p qembed-store --test reservation_sweeper_postgres -- --ignored --test-threads=1 2>&1 \
  | tee "$evidence_dir/rust-062-reservation-sweeper-canonical.log"
