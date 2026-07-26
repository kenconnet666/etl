#!/usr/bin/env bash
# End-to-end check of the DuckLake destination against the local stack in
# `.docker/local`: a Postgres 18 source, a separate Postgres 18 catalog, and
# rustfs for S3-compatible storage.
#
# The script drives the real `etl-replicator` binary, so it covers the parts the
# integration tests cannot: configuration loading, S3 storage, and a catalog on
# its own instance.
#
# Usage:
#   docker compose -f .docker/local/docker-compose.yml up -d
#   ./scripts/bin/e2e-ducklake.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

SOURCE_DSN="${SOURCE_DSN:-postgres://postgres:changeme@localhost:15432/postgres}"
CATALOG_DSN="${CATALOG_DSN:-postgres://lake_admin:changeme@localhost:15434/ducklake_catalog}"
# DuckLake attaches a Postgres catalog through libpq connection info rather than
# a URL, which is also how the destination builds its attach target.
CATALOG_CONNINFO="${CATALOG_CONNINFO:-host=localhost port=15434 dbname=ducklake_catalog user=lake_admin password=changeme}"
DUCKDB="${DUCKDB:-$HOME/.local/bin/duckdb}"
# Honour CARGO_TARGET_DIR so the freshly built binary is the one that runs.
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
REPLICATOR_BIN="$TARGET_DIR/release/etl-replicator"
TABLE="${TABLE:-e2e_orders}"
# The stack's default publication covers every table, so this check uses its own
# publication and pipeline id to stay isolated and fast.
PUBLICATION="${PUBLICATION:-e2e_ducklake_pub}"
PIPELINE_ID="${PIPELINE_ID:-9001}"
LOG_FILE="${LOG_FILE:-$REPO_ROOT/target/e2e-replicator.log}"
REPLICATOR_PID=""

log() {
  printf '\n=== %s ===\n' "$1"
}

cleanup() {
  if [[ -n "$REPLICATOR_PID" ]] && kill -0 "$REPLICATOR_PID" 2>/dev/null; then
    kill "$REPLICATOR_PID" 2>/dev/null || true
    wait "$REPLICATOR_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

require() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required command: $1" >&2
    exit 1
  }
}

require psql
require cargo
[[ -x "$DUCKDB" ]] || {
  echo "missing DuckDB CLI at $DUCKDB; set DUCKDB to its path" >&2
  exit 1
}

source_sql() {
  psql "$SOURCE_DSN" -v ON_ERROR_STOP=1 -q -c "$1"
}

lake_query() {
  # Setup statements print their own results, so the value under test is tagged
  # and extracted by tag instead of by position. A trailing semicolon is dropped
  # because the query is wrapped in an expression.
  local query
  query="$(printf '%s' "$1" | sed -e 's/[[:space:]]*;[[:space:]]*$//')"

  "$DUCKDB" -noheader -list -c "
    install ducklake; load ducklake;
    install postgres; load postgres;
    install httpfs; load httpfs;
    create or replace secret lake_storage (
      type s3, key_id 'minioadmin', secret 'minioadmin',
      endpoint 'localhost:19000', url_style 'path', use_ssl false
    );
    attach 'ducklake:postgres:${CATALOG_CONNINFO}' as lake (data_path 's3://lake/ducklake');
    select 'E2E_RESULT=' || coalesce(cast(($query) as varchar), '<null>');
  " 2>/dev/null | sed -n 's/^E2E_RESULT=//p'
}

# Waits until a lake query returns the expected value.
expect_lake() {
  local description="$1" query="$2" expected="$3" attempts="${4:-30}"
  local actual=""

  for _ in $(seq 1 "$attempts"); do
    actual="$(lake_query "$query" | tail -1 || true)"
    if [[ "$actual" == "$expected" ]]; then
      echo "ok: $description"
      return 0
    fi
    sleep 2
  done

  echo "FAILED: $description" >&2
  echo "  query:    $query" >&2
  echo "  expected: $expected" >&2
  echo "  actual:   $actual" >&2
  echo "  replicator log tail:" >&2
  tail -30 "$LOG_FILE" >&2 || true
  exit 1
}

log "resetting the source table"
source_sql "drop publication if exists $PUBLICATION"
source_sql "drop table if exists public.\"$TABLE\""
source_sql "drop table if exists public.\"${TABLE}_renamed\""
source_sql "create table public.\"$TABLE\" (
  id bigint primary key,
  customer text not null,
  quantity integer not null,
  amount numeric(10, 2)
)"
source_sql "create publication $PUBLICATION for table public.\"$TABLE\""
source_sql "insert into public.\"$TABLE\" values
  (1, 'alice', 2, 19.99),
  (2, 'bob', 1, 5.50)"

log "resetting replication state"
source_sql "drop schema if exists etl cascade"
psql "$CATALOG_DSN" -v ON_ERROR_STOP=1 -q -c "
  do \$\$
  declare
    table_name text;
  begin
    for table_name in
      select tablename from pg_tables
      where schemaname = 'public' and tablename like 'ducklake%'
    loop
      execute format('drop table if exists public.%I cascade', table_name);
    end loop;
  end
  \$\$;"
psql "$CATALOG_DSN" -v ON_ERROR_STOP=1 -q -c "drop schema if exists etl cascade"

log "building the replicator"
cargo build --release -p etl-replicator --features ducklake
[[ -x "$REPLICATOR_BIN" ]] || {
  echo "built binary not found at $REPLICATOR_BIN" >&2
  exit 1
}

# The build links against the prebuilt libduckdb rather than bundling DuckDB, so
# the library has to be on the loader path, exactly as the runtime image does it.
DUCKDB_LIB_DIR="$(dirname "$(find "$TARGET_DIR" -name libduckdb.so -print -quit)")"
[[ -n "$DUCKDB_LIB_DIR" ]] || {
  echo "libduckdb.so not found under $TARGET_DIR" >&2
  exit 1
}

log "starting the replicator"
APP_ENVIRONMENT=dev \
APP_PIPELINE__ID="$PIPELINE_ID" \
APP_PIPELINE__PUBLICATION_NAME="$PUBLICATION" \
LD_LIBRARY_PATH="$DUCKDB_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
RUST_LOG="${RUST_LOG:-info}" \
  "$REPLICATOR_BIN" > "$LOG_FILE" 2>&1 &
REPLICATOR_PID=$!
echo "replicator pid $REPLICATOR_PID, log $LOG_FILE"

expect_lake "initial copy landed two rows" \
  "select count(*) from lake.public.\"$TABLE\";" "2"

log "streaming an insert, an update, and a delete"
source_sql "insert into public.\"$TABLE\" values (3, 'carol', 4, 41.00)"
source_sql "update public.\"$TABLE\" set quantity = 7, amount = 70.70 where id = 1"
source_sql "delete from public.\"$TABLE\" where id = 2"

expect_lake "the replica reflects the current source state" \
  "select string_agg(id || ':' || customer || ':' || quantity, ',' order by id)
   from lake.public.\"$TABLE\";" \
  "1:alice:7,3:carol:4"

log "following DDL: add a column, rename a column, widen a type"
source_sql "alter table public.\"$TABLE\" add column channel text"
source_sql "alter table public.\"$TABLE\" rename column customer to buyer"
source_sql "alter table public.\"$TABLE\" alter column quantity type bigint"
source_sql "insert into public.\"$TABLE\" values (4, 'dave', 5000000000, 12.00, 'web')"

expect_lake "the added column replicated" \
  "select channel from lake.public.\"$TABLE\" where id = 4;" "web"
expect_lake "the renamed column replicated" \
  "select buyer from lake.public.\"$TABLE\" where id = 4;" "dave"
expect_lake "the widened type accepts values past the 32-bit range" \
  "select quantity from lake.public.\"$TABLE\" where id = 4;" "5000000000"
expect_lake "the destination column type followed the source" \
  "select data_type from information_schema.columns
   where table_catalog = 'lake' and table_schema = 'public'
     and table_name = '$TABLE' and column_name = 'quantity';" "BIGINT"

log "following DDL: rename the table"
source_sql "alter table public.\"$TABLE\" rename to \"${TABLE}_renamed\""
source_sql "insert into public.\"${TABLE}_renamed\" values (5, 'erin', 6, 6.00, 'app')"

expect_lake "the renamed table keeps its history and accepts new rows" \
  "select count(*) from lake.public.\"${TABLE}_renamed\";" "4"

log "truncating the source"
source_sql "truncate table public.\"${TABLE}_renamed\""

expect_lake "the truncate replicated" \
  "select count(*) from lake.public.\"${TABLE}_renamed\";" "0"

log "all end-to-end checks passed"
