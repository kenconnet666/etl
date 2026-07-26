#!/usr/bin/env bash
# Compares source row counts against the replica.
#
# The replica is disposable: when a table diverges, the fix is to copy it again
# with etl-resync rather than to repair it. This script is the matching
# detection step, because a divergence that nothing reports is a divergence you
# keep querying.
#
# Replication is asynchronous, so a difference on a table that is being written
# to only means the replica has not caught up yet. Compare a quiet table, or
# rerun and see whether the difference persists.
set -euo pipefail

usage() {
  cat <<'TEXT'
usage: verify-replica.sh --destination <ducklake|doris> [options]

options:
  --destination <kind>   Required. Which replica to compare against.
  --pipeline-id <id>     ETL pipeline id. Default: 9001.
  --table <schema.name>  Compare only this table. Repeatable.

environment:
  SOURCE_DSN             Source Postgres DSN.
                         Default: postgres://postgres:changeme@localhost:15432/postgres

  DuckLake:
    CATALOG_CONNINFO     libpq conninfo of the DuckLake catalog.
    LAKE_DATA_PATH       Data path of the lake. Default: s3://lake/ducklake
    S3_ENDPOINT          Default: localhost:19000
    S3_KEY_ID            Default: minioadmin
    S3_SECRET            Default: minioadmin
    DUCKDB               Path to the DuckDB CLI. Default: duckdb

  Doris:
    DORIS_HOST           Default: 127.0.0.1
    DORIS_MYSQL_PORT     Default: 19030
    DORIS_USER           Default: root
    DORIS_PASSWORD       Default: empty
    DORIS_DATABASE       Default: etl

Exits non-zero when any compared table differs.
TEXT
}

DESTINATION=""
PIPELINE_ID="${PIPELINE_ID:-9001}"
REQUESTED_TABLES=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --destination) DESTINATION="${2:?--destination needs a value}"; shift 2 ;;
    --pipeline-id) PIPELINE_ID="${2:?--pipeline-id needs a value}"; shift 2 ;;
    --table) REQUESTED_TABLES+=("${2:?--table needs a value}"); shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) echo "unexpected argument '$1'" >&2; usage >&2; exit 1 ;;
  esac
done

case "$DESTINATION" in
  ducklake|doris) ;;
  *) echo "--destination must be ducklake or doris" >&2; exit 1 ;;
esac

SOURCE_DSN="${SOURCE_DSN:-postgres://postgres:changeme@localhost:15432/postgres}"

require() {
  command -v "$1" >/dev/null 2>&1 || { echo "missing required command: $1" >&2; exit 1; }
}
require psql

source_query() {
  psql "$SOURCE_DSN" -v ON_ERROR_STOP=1 -qtAc "$1"
}

# Lists the tables the pipeline replicates, as `schema.name` per line.
list_tracked_tables() {
  source_query "
    select n.nspname || '.' || c.relname
    from etl.replication_state s
    join pg_class c on c.oid = s.table_id
    join pg_namespace n on n.oid = c.relnamespace
    where s.pipeline_id = $PIPELINE_ID and s.is_current
    order by 1"
}

source_count() {
  local schema="$1" name="$2"
  source_query "select count(*) from \"$schema\".\"$name\""
}

# --- DuckLake ---------------------------------------------------------------

DUCKDB="${DUCKDB:-duckdb}"
LAKE_DATA_PATH="${LAKE_DATA_PATH:-s3://lake/ducklake}"
S3_ENDPOINT="${S3_ENDPOINT:-localhost:19000}"
S3_KEY_ID="${S3_KEY_ID:-minioadmin}"
S3_SECRET="${S3_SECRET:-minioadmin}"

ducklake_count() {
  local schema="$1" name="$2"
  "$DUCKDB" -noheader -list -c "
    install ducklake; load ducklake;
    install postgres; load postgres;
    install httpfs; load httpfs;
    create or replace secret lake_storage (
      type s3, key_id '$S3_KEY_ID', secret '$S3_SECRET',
      endpoint '$S3_ENDPOINT', url_style 'path', use_ssl false
    );
    attach 'ducklake:postgres:${CATALOG_CONNINFO}' as lake (data_path '$LAKE_DATA_PATH');
    select 'COUNT=' || cast(count(*) as varchar) from lake.\"$schema\".\"$name\";
  " 2>/dev/null | sed -n 's/^COUNT=//p' | tail -1
}

# --- Doris ------------------------------------------------------------------

DORIS_HOST="${DORIS_HOST:-127.0.0.1}"
DORIS_MYSQL_PORT="${DORIS_MYSQL_PORT:-19030}"
DORIS_USER="${DORIS_USER:-root}"
DORIS_PASSWORD="${DORIS_PASSWORD:-}"
DORIS_DATABASE="${DORIS_DATABASE:-etl}"

doris_count() {
  local schema="$1" name="$2"
  local args=(-h "$DORIS_HOST" -P "$DORIS_MYSQL_PORT" -u "$DORIS_USER" -N -B)
  [[ -n "$DORIS_PASSWORD" ]] && args+=("-p$DORIS_PASSWORD")

  # The Doris destination folds the source schema into the table name.
  mysql "${args[@]}" -e \
    "select count(*) from \`$DORIS_DATABASE\`.\`${schema}_${name}\`" 2>/dev/null | tail -1
}

if [[ "$DESTINATION" == "ducklake" ]]; then
  : "${CATALOG_CONNINFO:?CATALOG_CONNINFO is required for the ducklake destination}"
  command -v "$DUCKDB" >/dev/null 2>&1 || require "$DUCKDB"
else
  require mysql
fi

# --- Comparison -------------------------------------------------------------

if [[ ${#REQUESTED_TABLES[@]} -gt 0 ]]; then
  TABLES=("${REQUESTED_TABLES[@]}")
else
  mapfile -t TABLES < <(list_tracked_tables)
fi

if [[ ${#TABLES[@]} -eq 0 ]]; then
  echo "pipeline $PIPELINE_ID tracks no tables" >&2
  exit 1
fi

printf '%-40s %12s %12s %10s\n' table source replica status
differences=0

for qualified in "${TABLES[@]}"; do
  schema="${qualified%%.*}"
  name="${qualified#*.}"

  expected="$(source_count "$schema" "$name" || true)"
  if [[ "$DESTINATION" == "ducklake" ]]; then
    actual="$(ducklake_count "$schema" "$name" || true)"
  else
    actual="$(doris_count "$schema" "$name" || true)"
  fi

  if [[ -z "$actual" ]]; then
    actual="missing"
  fi

  if [[ "$expected" == "$actual" ]]; then
    status="ok"
  else
    status="DIFFERS"
    differences=$((differences + 1))
  fi

  printf '%-40s %12s %12s %10s\n' "$qualified" "$expected" "$actual" "$status"
done

if [[ "$differences" -gt 0 ]]; then
  echo
  echo "$differences table(s) differ. If the difference persists, copy them again:"
  echo "  etl-resync --table-id \$(psql \"\$SOURCE_DSN\" -qtAc \"select 'schema.table'::regclass::oid\")"
  exit 1
fi

echo
echo "the replica matches the source"
