#!/usr/bin/env bash
set -euo pipefail

if ! docker compose version >/dev/null 2>&1; then
  echo "Docker Compose v2 is required to run PostgreSQL integration tests." >&2
  exit 1
fi

compose=(docker compose --project-name qembed-rust-test --file compose.test.yaml)
export DATABASE_URL="${DATABASE_URL:-postgres://qembed_test:qembed_test@127.0.0.1:55432/qembed_test}"
export NATS_URL="${NATS_URL:-nats://127.0.0.1:4222}"
evidence_dir="${TEST_EVIDENCE_DIR:-target/test-evidence}"
mkdir -p "$evidence_dir"

psql_in_test_db() {
  "${compose[@]}" exec -T postgres psql -U qembed_test -d qembed_test -v ON_ERROR_STOP=1 "$@"
}

install_rt050_verifier() {
  local postgres_version_num="$1"
  local sql_commit="a676013024e4592514415a6d7589b9ebbd6411af"
  local sql_repo_dir="${TMPDIR:-/tmp}/qembed-sql-rt050-${sql_commit}"

  if (( postgres_version_num < 190000 || postgres_version_num >= 200000 )); then
    echo "Skipping RT-050 C verifier fixture: PostgreSQL 19 is required (got $postgres_version_num)."
    return
  fi

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
  echo "RT-050 verifier installed from qembed-sql commit $sql_commit"
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
psql_in_test_db -c "CREATE EXTENSION IF NOT EXISTS vector"

postgres_version="$(psql_in_test_db -Atc 'SHOW server_version')"
postgres_version_num="$(psql_in_test_db -Atc "SHOW server_version_num")"
pgvector_version="$(psql_in_test_db -Atc "SELECT extversion FROM pg_extension WHERE extname = 'vector'")"
install_rt050_verifier "$postgres_version_num"
{
  echo "commit_sha=${GITHUB_SHA:-local}"
  echo "rt050_sql_commit=a676013024e4592514415a6d7589b9ebbd6411af"
  echo "rustc_version=$(rustc --version)"
  echo "postgres_version=$postgres_version"
  echo "postgres_version_num=$postgres_version_num"
  echo "pgvector_version=$pgvector_version"
} > "$evidence_dir/database-metadata.txt"

if cargo nextest --version >/dev/null 2>&1; then
  cargo nextest run --workspace --profile ci 2>&1 | tee "$evidence_dir/nextest.log"
  cargo test --workspace --doc 2>&1 | tee "$evidence_dir/doc-tests.log"
else
  cargo test --workspace 2>&1 | tee "$evidence_dir/cargo-test.log"
fi

cargo test -p qembed-api --test rust_051c_nats_reconnect -- --ignored --test-threads=1 2>&1 \
  | tee "$evidence_dir/nats-reconnect.log"
