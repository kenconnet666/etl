#!/usr/bin/env bash
# Measures replication throughput against the local stack.
#
# Reported throughput is `rows * 1000 / elapsed_ms`, floored. Each scenario names
# exactly what the clock covers, because the numbers are not comparable
# otherwise:
#
# - catchup:   the replicator is stopped, rows are written to the source to build
#              a backlog, then the clock runs from replicator start until the
#              destination count matches. Includes reconnect and a cold table.
# - streaming: the replicator is already running; the clock covers the source
#              write and the wait for the destination to match.
# - warm:      like streaming, but the table already holds the rows being
#              updated or deleted.
#
# Source write time is reported separately and is not subtracted, so a streaming
# number is an end-to-end figure rather than a destination-only one.
#
# The lake writes through the local stack's S3 endpoint, so object-storage
# round trips are included. Point LAKE_DATA_PATH elsewhere to change that.
set -euo pipefail

DESTINATION="${DESTINATION:-ducklake}"
ROWS="${ROWS:-100000}"
TABLES="${TABLES:-4}"
SOURCE_DSN="${SOURCE_DSN:-postgres://postgres:changeme@localhost:15432/postgres}"
CATALOG_DSN="${CATALOG_DSN:-postgres://lake_admin:changeme@localhost:15434/ducklake_catalog}"
CATALOG_CONNINFO="${CATALOG_CONNINFO:-host=localhost port=15434 dbname=ducklake_catalog user=lake_admin password=changeme}"
DUCKDB="${DUCKDB:-duckdb}"
LAKE_DATA_PATH="${LAKE_DATA_PATH:-s3://lake/ducklake}"
# S3 settings only apply to an s3:// data path.
LAKE_IS_S3=0
[[ "$LAKE_DATA_PATH" == s3://* ]] && LAKE_IS_S3=1
LAKE_SECRET_SQL=""
if [[ "$LAKE_IS_S3" -eq 1 ]]; then
  LAKE_SECRET_SQL="create or replace secret lake_storage (type s3, key_id 'minioadmin',
    secret 'minioadmin', endpoint 'localhost:19000', url_style 'path', use_ssl false);"
fi
PIPELINE_ID="${PIPELINE_ID:-9101}"
PUBLICATION="${PUBLICATION:-etl_bench_pub}"
TABLE_PREFIX="${TABLE_PREFIX:-bench_orders}"
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
REPLICATOR_BIN="$TARGET_DIR/release/etl-replicator"
LOG_FILE="$TARGET_DIR/bench-replicator.log"
DORIS_HOST="${DORIS_HOST:-127.0.0.1}"
DORIS_MYSQL_PORT="${DORIS_MYSQL_PORT:-19030}"
DORIS_HTTP_PORT="${DORIS_HTTP_PORT:-18030}"
DORIS_USER="${DORIS_USER:-root}"
DORIS_PASSWORD="${DORIS_PASSWORD:-}"
DORIS_DATABASE="${DORIS_DATABASE:-etl_bench}"

REPLICATOR_PID=""

log() { printf '\n=== %s ===\n' "$1"; }

cleanup() {
  if [[ -n "$REPLICATOR_PID" ]] && kill -0 "$REPLICATOR_PID" 2>/dev/null; then
    kill "$REPLICATOR_PID" 2>/dev/null || true
    wait "$REPLICATOR_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

require() {
  command -v "$1" >/dev/null 2>&1 || { echo "missing required command: $1" >&2; exit 1; }
}
require psql
require cargo

source_sql() { psql "$SOURCE_DSN" -v ON_ERROR_STOP=1 -q -c "$1"; }
now_ms() { date +%s%3N; }

# Returns the destination row count for one table.
#
# The polling query only loads the extensions rather than installing them, since
# installing costs about a second and would land inside every measurement that
# waits for a result.
dest_count() {
  local table="$1"
  if [[ "$DESTINATION" == "ducklake" ]]; then
    "$DUCKDB" -noheader -list -c "
      load ducklake; load postgres; load httpfs;
      $LAKE_SECRET_SQL
      attach 'ducklake:postgres:${CATALOG_CONNINFO}' as lake (data_path '$LAKE_DATA_PATH', override_data_path true);
      select 'N=' || cast(count(*) as varchar) from lake.public.\"$table\";
    " 2>/dev/null | sed -n 's/^N=//p' | tail -1
  else
    local args=(-h "$DORIS_HOST" -P "$DORIS_MYSQL_PORT" -u "$DORIS_USER" -N -B)
    [[ -n "$DORIS_PASSWORD" ]] && args+=("-p$DORIS_PASSWORD")
    mysql "${args[@]}" -e \
      "select count(*) from \`$DORIS_DATABASE\`.\`public_$table\`" 2>/dev/null | tail -1
  fi
}

# Waits until a table reaches an expected count, then prints the elapsed ms.
# Prints `timeout` when the deadline passes.
wait_for_count() {
  local table="$1" expected="$2" timeout_s="${3:-300}"
  local start deadline actual
  start="$(now_ms)"
  deadline=$((start + timeout_s * 1000))
  while :; do
    actual="$(dest_count "$table" || true)"
    if [[ "$actual" == "$expected" ]]; then
      echo "$(( $(now_ms) - start ))"
      return 0
    fi
    if [[ "$(now_ms)" -gt "$deadline" ]]; then
      echo "timeout(last=$actual expected=$expected)"
      return 1
    fi
    sleep 0.2
  done
}

# Prints one result row of the report.
report() {
  printf '%-22s %10s %12s %12s %12s\n' "$1" "$2" "$3" "$4" "$5"
}

throughput() {
  local rows="$1" elapsed_ms="$2"
  if [[ "$elapsed_ms" =~ ^[0-9]+$ ]] && [[ "$elapsed_ms" -gt 0 ]]; then
    echo $((rows * 1000 / elapsed_ms))
  else
    echo "-"
  fi
}

start_replicator() {
  local extra_env=(APP_CONFIG_DIR="$CONFIG_DIR")
  if [[ "$DESTINATION" == "ducklake" ]]; then
    extra_env+=(LD_LIBRARY_PATH="$DUCKDB_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}")
  fi

  env APP_ENVIRONMENT=dev \
      APP_PIPELINE__ID="$PIPELINE_ID" \
      APP_PIPELINE__PUBLICATION_NAME="$PUBLICATION" \
      RUST_LOG="${RUST_LOG:-info}" \
      "${extra_env[@]}" \
      "$REPLICATOR_BIN" >> "$LOG_FILE" 2>&1 &
  REPLICATOR_PID=$!
}

stop_replicator() {
  if [[ -n "$REPLICATOR_PID" ]] && kill -0 "$REPLICATOR_PID" 2>/dev/null; then
    kill "$REPLICATOR_PID" 2>/dev/null || true
    wait "$REPLICATOR_PID" 2>/dev/null || true
  fi
  REPLICATOR_PID=""
}

# --- Setup ------------------------------------------------------------------

log "resetting the source"
source_sql "drop publication if exists $PUBLICATION"
for i in $(seq 1 "$TABLES"); do
  source_sql "drop table if exists public.\"${TABLE_PREFIX}_$i\""
  source_sql "create table public.\"${TABLE_PREFIX}_$i\" (
    id bigint primary key,
    customer text not null,
    quantity integer not null,
    amount numeric(10, 2),
    payload jsonb,
    created_at timestamptz not null default now()
  )"
done
TABLE_LIST=""
for i in $(seq 1 "$TABLES"); do
  [[ -n "$TABLE_LIST" ]] && TABLE_LIST+=", "
  TABLE_LIST+="public.\"${TABLE_PREFIX}_$i\""
done
source_sql "create publication $PUBLICATION for table $TABLE_LIST"

log "resetting replication and destination state"
source_sql "drop schema if exists etl cascade"
if [[ "$DESTINATION" == "ducklake" ]]; then
  psql "${CATALOG_DSN:-postgres://lake_admin:changeme@localhost:15434/ducklake_catalog}" \
    -v ON_ERROR_STOP=1 -q -c "
    do \$\$
    declare t text;
    begin
      for t in select tablename from pg_tables
               where schemaname = 'public' and tablename like 'ducklake%'
      loop execute format('drop table if exists public.%I cascade', t); end loop;
    end \$\$;"
else
  args=(-h "$DORIS_HOST" -P "$DORIS_MYSQL_PORT" -u "$DORIS_USER" -N -B)
  [[ -n "$DORIS_PASSWORD" ]] && args+=("-p$DORIS_PASSWORD")
  mysql "${args[@]}" -e "drop database if exists \`$DORIS_DATABASE\`" 2>/dev/null || true
fi

log "building the replicator"
if [[ "$DESTINATION" == "ducklake" ]]; then
  cargo build --release -p etl-replicator --features ducklake --bins
  DUCKDB_LIB_DIR="$(dirname "$(find "$TARGET_DIR" -name libduckdb.so -print -quit)")"
  CONFIG_DIR="$TARGET_DIR/bench-ducklake-configuration"
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
  ducklake:
    catalog_url: postgres://lake_admin:changeme@localhost:15434/ducklake_catalog
    data_path: $LAKE_DATA_PATH
    maintenance_mode: postgres
YAML
  if [[ "$LAKE_IS_S3" -eq 1 ]]; then
    cat >> "$CONFIG_DIR/dev.yaml" <<YAML
    s3_access_key_id: minioadmin
    s3_secret_access_key: minioadmin
    s3_region: us-east-1
    s3_endpoint: localhost:19000
    s3_url_style: path
    s3_use_ssl: false
YAML
  fi
  [[ "$LAKE_IS_S3" -eq 0 ]] && { rm -rf "$LAKE_DATA_PATH"; mkdir -p "$LAKE_DATA_PATH"; }
else
  cargo build --release -p etl-replicator --features doris --no-default-features
  CONFIG_DIR="$TARGET_DIR/bench-doris-configuration"
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
fi
: > "$LOG_FILE"

# Generates `count` rows into one table starting at `offset`.
fill() {
  local table="$1" offset="$2" count="$3"
  source_sql "insert into public.\"$table\"
    select $offset + i,
           'customer_' || (i % 997),
           (i % 50)::integer,
           ((i % 10000) / 100.0)::numeric(10,2),
           jsonb_build_object('tier', case when i % 3 = 0 then 'gold' else 'silver' end,
                              'limits', jsonb_build_object('daily', i % 100)),
           now()
    from generate_series(1, $count) g(i)"
}

if [[ "$DESTINATION" == "ducklake" ]]; then
  # Install once so the polling queries only have to load.
  "$DUCKDB" -c "install ducklake; install postgres; install httpfs;" > /dev/null 2>&1 || true
fi

echo
echo "destination=$DESTINATION rows=$ROWS tables=$TABLES"
echo
report "scenario" "rows" "source_ms" "replica_ms" "rows/s"

# --- 1. catchup: backlog first, then start the replicator -------------------

TABLE_1="${TABLE_PREFIX}_1"
start="$(now_ms)"
fill "$TABLE_1" 0 "$ROWS"
source_ms=$(( $(now_ms) - start ))

start_replicator
elapsed="$(wait_for_count "$TABLE_1" "$ROWS" 600 || true)"
report "catchup" "$ROWS" "$source_ms" "$elapsed" "$(throughput "$ROWS" "$elapsed")"

# --- 2. streaming insert ----------------------------------------------------

start="$(now_ms)"
fill "$TABLE_1" "$ROWS" "$ROWS"
source_ms=$(( $(now_ms) - start ))
elapsed="$(wait_for_count "$TABLE_1" $((ROWS * 2)) 600 || true)"
report "streaming insert" "$ROWS" "$source_ms" "$elapsed" "$(throughput "$ROWS" "$elapsed")"

# --- 3. warm update ---------------------------------------------------------

# The row count does not change, so the wait targets a value instead.
wait_for_value() {
  local table="$1" query="$2" expected="$3" timeout_s="${4:-300}"
  local start deadline actual
  start="$(now_ms)"
  deadline=$((start + timeout_s * 1000))
  while :; do
    if [[ "$DESTINATION" == "ducklake" ]]; then
      actual="$("$DUCKDB" -noheader -list -c "
        install ducklake; load ducklake; install postgres; load postgres;
        install httpfs; load httpfs;
        $LAKE_SECRET_SQL
        attach 'ducklake:postgres:${CATALOG_CONNINFO}' as lake (data_path '$LAKE_DATA_PATH', override_data_path true);
        select 'N=' || cast(($query) as varchar);
      " 2>/dev/null | sed -n 's/^N=//p' | tail -1 || true)"
    else
      local args=(-h "$DORIS_HOST" -P "$DORIS_MYSQL_PORT" -u "$DORIS_USER" -N -B)
      [[ -n "$DORIS_PASSWORD" ]] && args+=("-p$DORIS_PASSWORD")
      actual="$(mysql "${args[@]}" -e "select $query" 2>/dev/null | tail -1 || true)"
    fi
    [[ "$actual" == "$expected" ]] && { echo "$(( $(now_ms) - start ))"; return 0; }
    if [[ "$(now_ms)" -gt "$deadline" ]]; then
      echo "timeout(last=$actual expected=$expected)"
      return 1
    fi
    sleep 0.2
  done
}

start="$(now_ms)"
source_sql "update public.\"$TABLE_1\" set quantity = 777 where id <= $ROWS"
source_ms=$(( $(now_ms) - start ))
if [[ "$DESTINATION" == "ducklake" ]]; then
  query="select count(*) from lake.public.\"$TABLE_1\" where quantity = 777"
else
  query="count(*) from \`$DORIS_DATABASE\`.\`public_$TABLE_1\` where quantity = 777"
fi
elapsed="$(wait_for_value "$TABLE_1" "$query" "$ROWS" 600 || true)"
report "warm update" "$ROWS" "$source_ms" "$elapsed" "$(throughput "$ROWS" "$elapsed")"

# --- 4. warm delete ---------------------------------------------------------

DELETE_ROWS=$((ROWS / 2))
start="$(now_ms)"
source_sql "delete from public.\"$TABLE_1\" where id <= $DELETE_ROWS"
source_ms=$(( $(now_ms) - start ))
elapsed="$(wait_for_count "$TABLE_1" $((ROWS * 2 - DELETE_ROWS)) 600 || true)"
report "warm delete" "$DELETE_ROWS" "$source_ms" "$elapsed" "$(throughput "$DELETE_ROWS" "$elapsed")"

# --- 5. multi-table insert --------------------------------------------------

PER_TABLE=$((ROWS / TABLES))
start="$(now_ms)"
for i in $(seq 2 "$TABLES"); do
  fill "${TABLE_PREFIX}_$i" 0 "$PER_TABLE"
done
source_ms=$(( $(now_ms) - start ))
multi_start="$(now_ms)"
multi_failed=0
for i in $(seq 2 "$TABLES"); do
  wait_for_count "${TABLE_PREFIX}_$i" "$PER_TABLE" 600 > /dev/null || multi_failed=1
done
elapsed=$(( $(now_ms) - multi_start ))
[[ "$multi_failed" -eq 1 ]] && elapsed="timeout"
multi_rows=$((PER_TABLE * (TABLES - 1)))
report "multi-table insert" "$multi_rows" "$source_ms" "$elapsed" \
  "$(throughput "$multi_rows" "$elapsed")"

# --- 6. interleaved insert, update, and delete ------------------------------

# Real change streams mix operations, and a destination that groups by operation
# type has to close a group on every switch. A single-operation scenario cannot
# show that, so this one interleaves all three inside one transaction.
MIXED_ROWS=$((ROWS / 2))
MIXED_BASE=$((ROWS * 4))
start="$(now_ms)"
source_sql "do \$\$
  declare i bigint;
begin
  for i in 1..$MIXED_ROWS loop
    insert into public.\"$TABLE_1\" values
      ($MIXED_BASE + i, 'mixed_' || i, 1, 1.00,
       jsonb_build_object('tier', 'silver'), now());
    update public.\"$TABLE_1\" set quantity = 2 where id = $MIXED_BASE + i;
    if i % 2 = 0 then
      delete from public.\"$TABLE_1\" where id = $MIXED_BASE + i;
    end if;
  end loop;
end \$\$"
source_ms=$(( $(now_ms) - start ))

# Half the inserted rows survive, since every second one is deleted again.
mixed_expected=$((ROWS * 2 - DELETE_ROWS + MIXED_ROWS / 2))
elapsed="$(wait_for_count "$TABLE_1" "$mixed_expected" 600 || true)"
report "interleaved i/u/d" "$MIXED_ROWS" "$source_ms" "$elapsed" \
  "$(throughput "$MIXED_ROWS" "$elapsed")"

log "batch stage distribution"
# The replicator exports Prometheus metrics on 9000; the stage histogram shows
# where a batch spends its time instead of leaving it to guesswork.
if command -v curl >/dev/null 2>&1; then
  curl -s http://127.0.0.1:9000/metrics 2>/dev/null \
    | grep -E 'etl_ducklake_batch_stage_duration_seconds|etl_ducklake_(upsert_rows|delete_predicates)|etl_ducklake_pool_checkout_wait|etl_ducklake_blocking_slot_wait' \
    | grep -vE '_bucket|^#' \
    | sed 's/etl_ducklake_//' \
    || echo "no stage metrics scraped"
else
  echo "curl is missing, skipping the stage distribution"
fi

log "analytical queries on the replica"
if [[ "$DESTINATION" == "ducklake" ]]; then
  for q in "select count(*) from lake.public.\"$TABLE_1\"" \
           "select customer, sum(amount) s from lake.public.\"$TABLE_1\" group by customer order by s desc limit 5" \
           "select count(*) from lake.public.\"$TABLE_1\" where cast(payload['tier'] as varchar) = 'gold'"; do
    start="$(now_ms)"
    "$DUCKDB" -noheader -list -c "
      install ducklake; load ducklake; install postgres; load postgres;
      install httpfs; load httpfs;
      $LAKE_SECRET_SQL
      attach 'ducklake:postgres:${CATALOG_CONNINFO}' as lake (data_path '$LAKE_DATA_PATH', override_data_path true);
      $q;" > /dev/null 2>&1
    printf '%-70s %6s ms\n' "${q:0:70}" "$(( $(now_ms) - start ))"
  done
else
  args=(-h "$DORIS_HOST" -P "$DORIS_MYSQL_PORT" -u "$DORIS_USER" -N -B)
  [[ -n "$DORIS_PASSWORD" ]] && args+=("-p$DORIS_PASSWORD")
  for q in "select count(*) from \`$DORIS_DATABASE\`.\`public_$TABLE_1\`" \
           "select customer, sum(amount) s from \`$DORIS_DATABASE\`.\`public_$TABLE_1\` group by customer order by s desc limit 5" \
           "select count(*) from \`$DORIS_DATABASE\`.\`public_$TABLE_1\` where cast(payload['tier'] as string) = 'gold'"; do
    start="$(now_ms)"
    mysql "${args[@]}" -e "$q" > /dev/null 2>&1
    printf '%-70s %6s ms\n' "${q:0:70}" "$(( $(now_ms) - start ))"
  done
fi

stop_replicator
log "benchmark finished"
