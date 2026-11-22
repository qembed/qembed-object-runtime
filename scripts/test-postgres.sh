#!/usr/bin/env bash
set -euo pipefail

if ! docker compose version >/dev/null 2>&1; then
  echo "Docker Compose v2 is required to run PostgreSQL integration tests." >&2
  exit 1
fi

compose=(docker compose --project-name qembed-rust-test --file compose.test.yaml)
export DATABASE_URL="${DATABASE_URL:-postgres://qembed_test:qembed_test@127.0.0.1:55432/qembed_test}"
export NATS_URL="${NATS_URL:-nats://127.0.0.1:4222}"
export VALKEY_URL="${VALKEY_URL:-redis://127.0.0.1:6379}"
evidence_dir="${TEST_EVIDENCE_DIR:-target/test-evidence}"
mkdir -p "$evidence_dir"

psql_in_test_db() {
  "${compose[@]}" exec -T postgres psql -U qembed_test -d qembed_test -v ON_ERROR_STOP=1 "$@"
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
{
  echo "commit_sha=${GITHUB_SHA:-local}"
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
