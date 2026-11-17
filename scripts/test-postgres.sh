#!/usr/bin/env bash
set -euo pipefail

if ! docker compose version >/dev/null 2>&1; then
  echo "Docker Compose v2 is required to run PostgreSQL integration tests." >&2
  exit 1
fi

compose=(docker compose --project-name qembed-rust-test --file compose.test.yaml)
export DATABASE_URL="${DATABASE_URL:-postgres://qembed_test:qembed_test@127.0.0.1:55432/qembed_test}"

cleanup() {
  "${compose[@]}" down --volumes --remove-orphans
}
trap cleanup EXIT

"${compose[@]}" up --detach --wait
cargo test --workspace
