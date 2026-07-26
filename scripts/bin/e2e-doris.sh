#!/usr/bin/env bash
# End-to-end check of the Doris destination against the local stack in
# `.docker/local`: a Postgres 18 source and a single-node Doris cluster.
#
# The script drives the real `etl-replicator` binary, so it covers Stream Load,
# label-based idempotency, merge-on-write deletes, and DDL over the MySQL
# protocol, none of which the unit tests can reach.
#
# Usage:
#   docker compose -f .docker/local/docker-compose.yml up -d
#   ./scripts/bin/e2e-doris.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

SOURCE_DSN="${SOURCE_DSN:-postgres://postgres:changeme@localhost:15432/postgres}"
DORIS_HOST="${DORIS_HOST:-127.0.0.1}"
DORIS_MYSQL_PORT="${DORIS_MYSQL_PORT:-19030}"
DORIS_HTTP_PORT="${DORIS_HTTP_PORT:-18030}"
DORIS_USER="${DORIS_USER:-root}"
DORIS_PASSWORD="${DORIS_PASSWORD:-}"
DORIS_DATABASE="${DORIS_DATABASE:-etl_e2e}"
TABLE="${TABLE:-e2e_doris_orders}"
DORIS_TABLE="public_${TABLE}"
PUBLICATION="${PUBLICATION:-e2e_doris_pub}"
PIPELINE_ID="${PIPELINE_ID:-9002}"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
REPLICATOR_BIN="$TARGET_DIR/release/etl-replicator"
LOG_FILE="${LOG_FILE:-$TARGET_DIR/e2e-doris-replicator.log}"
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

command -v psql >/dev/null || { echo "missing psql" >&2; exit 1; }
command -v mysql >/dev/null || { echo "missing mysql client" >&2; exit 1; }

source_sql() {
  psql "$SOURCE_DSN" -v ON_ERROR_STOP=1 -q -c "$1"
}

doris_sql() {
  if [[ -n "$DORIS_PASSWORD" ]]; then
    mysql -h "$DORIS_HOST" -P "$DORIS_MYSQL_PORT" -u "$DORIS_USER" \
      -p"$DORIS_PASSWORD" -N -B -e "$1"
  else
    mysql -h "$DORIS_HOST" -P "$DORIS_MYSQL_PORT" -u "$DORIS_USER" -N -B -e "$1"
  fi
}

# Waits until a Doris query returns the expected value.
expect_doris() {
  local description="$1" query="$2" expected="$3" attempts="${4:-30}"
  local actual=""

  for _ in $(seq 1 "$attempts"); do
    actual="$(doris_sql "$query" 2>/dev/null | tail -1 || true)"
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

log "checking that the Doris cluster is alive"
if ! doris_sql "select 1" >/dev/null 2>&1; then
  echo "Doris is not reachable on $DORIS_HOST:$DORIS_MYSQL_PORT; start it with" >&2
  echo "  docker compose -f .docker/local/docker-compose.yml up -d doris-fe doris-be" >&2
  exit 1
fi
doris_sql "select \`host\`, \`alive\` from backends()"

log "resetting the source table"
source_sql "drop publication if exists $PUBLICATION"
source_sql "drop table if exists public.\"$TABLE\""
source_sql "create table public.\"$TABLE\" (
  id bigint primary key,
  customer text not null,
  quantity integer not null,
  amount numeric(10, 2),
  tags text[],
  scores numeric(6, 2)[]
)"
source_sql "create publication $PUBLICATION for table public.\"$TABLE\""
source_sql "insert into public.\"$TABLE\" values
  (1, 'alice', 2, 19.99, array['vip','eu'], array[1.50, 2.25]),
  (2, 'bob', 1, 5.50, null, null)"

log "resetting replication and destination state"
source_sql "drop schema if exists etl cascade"
doris_sql "drop database if exists \`$DORIS_DATABASE\`" || true

log "building the replicator"
cargo build --release -p etl-replicator --features doris --no-default-features
[[ -x "$REPLICATOR_BIN" ]] || { echo "built binary not found at $REPLICATOR_BIN" >&2; exit 1; }

log "writing the Doris configuration"
# The shipped dev configuration targets DuckLake, and a destination is an
# externally tagged enum, so this run gets its own configuration directory.
CONFIG_DIR="$TARGET_DIR/e2e-doris-configuration"
mkdir -p "$CONFIG_DIR"
cat > "$CONFIG_DIR/base.yaml" <<YAML
pipeline:
  id: $PIPELINE_ID
  publication_name: $PUBLICATION
YAML
cat > "$CONFIG_DIR/dev.yaml" <<YAML
pipeline:
  id: $PIPELINE_ID
  publication_name: $PUBLICATION
  pg_connection:
    host: localhost
    port: 15432
    name: postgres
    username: postgres
    password: changeme
    tls:
      enabled: false
      trusted_root_certs: ""

destination:
  doris:
    fe_http_url: http://$DORIS_HOST:$DORIS_HTTP_PORT
    fe_mysql_host: $DORIS_HOST
    fe_mysql_port: $DORIS_MYSQL_PORT
    user: $DORIS_USER
    password: "$DORIS_PASSWORD"
    database: $DORIS_DATABASE
    replication_num: 1
YAML

log "starting the replicator"
APP_ENVIRONMENT=dev \
APP_CONFIG_DIR="$CONFIG_DIR" \
RUST_LOG="${RUST_LOG:-info}" \
  "$REPLICATOR_BIN" > "$LOG_FILE" 2>&1 &
REPLICATOR_PID=$!
echo "replicator pid $REPLICATOR_PID, log $LOG_FILE"

expect_doris "initial copy landed two rows" \
  "select count(*) from \`$DORIS_DATABASE\`.\`$DORIS_TABLE\`" "2" 60

log "streaming an insert, an update, and a delete"
source_sql "insert into public.\"$TABLE\" values (3, 'carol', 4, 41.00)"
source_sql "update public.\"$TABLE\" set quantity = 7 where id = 1"
source_sql "delete from public.\"$TABLE\" where id = 2"

expect_doris "the replica reflects the current source state" \
  "select group_concat(concat(id, ':', customer, ':', quantity) order by id)
   from \`$DORIS_DATABASE\`.\`$DORIS_TABLE\`" \
  "1:alice:7,3:carol:4"

expect_doris "arrays land in a native array column" \
  "select array_size(tags) from \`$DORIS_DATABASE\`.\`$DORIS_TABLE\` where id = 1" "2"
expect_doris "an array element keeps its declared precision" \
  "select array_sum(scores) from \`$DORIS_DATABASE\`.\`$DORIS_TABLE\` where id = 1" "3.75"
expect_doris "the array column type is native" \
  "select lower(data_type) from information_schema.columns
   where table_schema = '$DORIS_DATABASE' and table_name = '$DORIS_TABLE'
     and column_name = 'tags'" "array"

log "changing a primary key value"
source_sql "update public.\"$TABLE\" set id = 30 where id = 3"

# Postgres keeps the row identity in the old image, so the replica has to drop
# the row it still holds under the previous key.
expect_doris "the row moved to the new key without leaving a ghost" \
  "select group_concat(cast(id as string) order by id)
   from \`$DORIS_DATABASE\`.\`$DORIS_TABLE\`" \
  "1,30"

log "following DDL: add a column and widen a type"
source_sql "alter table public.\"$TABLE\" add column channel text"
source_sql "alter table public.\"$TABLE\" alter column quantity type bigint"
source_sql "insert into public.\"$TABLE\" values
  (4, 'dave', 5000000000, 12.00, null, null, 'web')"

expect_doris "the added column replicated" \
  "select channel from \`$DORIS_DATABASE\`.\`$DORIS_TABLE\` where id = 4" "web"
expect_doris "the widened type accepts values past the 32-bit range" \
  "select quantity from \`$DORIS_DATABASE\`.\`$DORIS_TABLE\` where id = 4" "5000000000"

log "truncating the source"
source_sql "truncate table public.\"$TABLE\""

expect_doris "the truncate replicated" \
  "select count(*) from \`$DORIS_DATABASE\`.\`$DORIS_TABLE\`" "0"

log "all end-to-end checks passed"
