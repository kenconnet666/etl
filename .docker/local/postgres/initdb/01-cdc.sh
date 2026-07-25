#!/usr/bin/env bash
# Source DB CDC setup: runs once via docker-entrypoint-initdb.d on first start.
set -euo pipefail

psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" <<-EOSQL
    -- CDC replication role: logical replication + read-all-data
    CREATE ROLE dbuser_cdc REPLICATION LOGIN PASSWORD '${POSTGRES_PASSWORD}' CONNECTION LIMIT 8;
    GRANT pg_read_all_data TO dbuser_cdc;

    -- Publish all tables (new tables included automatically)
    CREATE PUBLICATION dbz_publication FOR ALL TABLES;
EOSQL

# Allow replication connections from all hosts
cat >> "$PGDATA/pg_hba.conf" <<-EOF
host replication dbuser_cdc 0.0.0.0/0 scram-sha-256
EOF
