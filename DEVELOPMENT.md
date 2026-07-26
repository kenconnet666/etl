# Development Guide

## Prerequisites

- **Rust** from `rust-toolchain.toml`, installed through [rustup](https://rustup.rs/).
- **Docker Compose** for the local Postgres, DuckLake catalog, and object storage.
- **PostgreSQL client** (`psql`) for migrations.
- **cargo-nextest** for the test suite.
- **SQLx CLI** for running migrations by hand:

  ```bash
  cargo install sqlx-cli --version 0.9.0-alpha.1 \
    --no-default-features --features rustls,postgres --locked
  ```

DuckDB is linked from the official prebuilt library instead of compiling the
bundled sources. `.cargo/config.toml` sets `DUCKDB_DOWNLOAD_LIB=1`, so the first
build downloads the library once into `<target-dir>/duckdb-download` and reuses
it afterwards.

### Working from Windows

The toolchain runs inside WSL2 while the repository can live on the Windows side.
That works, but two things are worth setting up because they cost real time:

Point `CARGO_TARGET_DIR` at a path inside the Linux filesystem. A target
directory on `/mnt/c` makes every build and test run several times slower, and
the DuckDB library download lands there too.

```bash
# ~/.etl-dev.env, sourced by every helper script
export ETL_REPO_DIR=/mnt/c/path/to/etl
export CARGO_TARGET_DIR="$HOME/.cache/etl-target"
export ETL_DUCKDB_EXTENSION_ROOT="$ETL_REPO_DIR/vendor/duckdb/extensions"
export TESTS_DATABASE_HOST=localhost
export TESTS_DATABASE_PORT=5430
export TESTS_DATABASE_USERNAME=postgres
export TESTS_DATABASE_PASSWORD=postgres
```

Keep the working tree on LF. `git config core.autocrlf input` avoids a diff that
touches every file, and the formatter check in CI fails on CRLF.

Long-running work should not be a child of `wsl.exe`: the process is killed when
the launching shell exits. Run it under systemd instead, writing its log to a
path inside the Linux filesystem.

```bash
systemd-run --unit=etl-bench --collect --setenv=HOME=/root bash /path/to/script.sh
journalctl -u etl-bench --no-pager | tail
```

## Task runner

Common tasks live in `crates/xtask`, reachable through the `cargo x` alias.

```bash
cargo x fmt              # format with the pinned nightly rustfmt
cargo x fmt --check      # check formatting
cargo x check            # pre-PR gate: fmt, sort, clippy
cargo x fix              # auto-fix: clippy --fix, fmt, sort
cargo x msrv             # verify MSRV consistency
cargo x migrate          # run source and store migrations
cargo x postgres start   # start the test Postgres clusters
cargo x seed             # seed a database with example tables
cargo x example ducklake # run the DuckLake example
cargo x vendor-duckdb    # download DuckDB extensions
cargo x nextest run      # full sharded test suite
```

Formatting is the only workflow that uses nightly Rust, because the repository
relies on nightly-only `rustfmt` options. It is pinned to `nightly-2026-04-15`
and can be overridden with `RUSTFMT_NIGHTLY_TOOLCHAIN`.

## Test Postgres clusters

```bash
cargo x postgres start
```

This starts sharded Postgres clusters from `scripts/docker/docker-compose.yaml`.
The first primary listens on `localhost:5430` and its physical read replica on
`localhost:6430`; additional shards use consecutive ports with the same `+1000`
replica offset. The read replica exists because some tests create logical slots
on a standby.

Postgres 18 containers store data under `/var/lib/postgresql/<major>/data`, so
the compose file mounts the parent directory.

TLS is enabled by default. The task runner generates a local test CA and server
certificate under `target/postgres-tls/` and copies them into the containers.
Clients may still connect without TLS; set `TESTS_DATABASE_TLS_ENABLED=true` to
require verified TLS.

Environment variables:

| Variable | Default | Description |
| --- | --- | --- |
| `POSTGRES_USER` | `postgres` | Database user |
| `POSTGRES_PASSWORD` | `postgres` | Database password |
| `POSTGRES_DB` | `postgres` | Database name |
| `POSTGRES_PORT` | `5430` | First primary port |
| `POSTGRES_REPLICA_PORT` | `6430` | First replica port |
| `NUM_LOCAL_DATABASES` | `3` | Number of shards to start |
| `POSTGRES_DATA_VOLUME` | (empty) | Host path for persistent primary storage |
| `POSTGRES_REPLICA_DATA_VOLUME` | (empty) | Host path for persistent replica storage |

## End-to-end stack

`.docker/local` runs the topology the replicator targets in production: a
Postgres 18 source with logical replication, a separate Postgres 18 instance for
the DuckLake catalog, and rustfs for S3-compatible storage.

```bash
docker compose -f .docker/local/docker-compose.yml up -d
```

| Service | Port | Purpose |
| --- | --- | --- |
| `source-pg` | 15432 | Replication source |
| `catalog-pg` | 15434 | DuckLake catalog |
| `rustfs` | 19000 / 19001 | S3 API and console |
| `doris-fe` | 18030 / 19030 | Doris HTTP (Stream Load) and MySQL protocol |
| `doris-be` | — | Doris backend |

Copy `.docker/local/.env.example` to `.docker/local/.env` to override the
generated passwords.

The Doris containers need static addresses because the image entrypoint rejects
hostnames in `FE_SERVERS`, and `priority_networks` must name the container
network because WSL2 gives each distribution its own loopback namespace. The
official images assume the JVM can read container limits through cgroups; where
that fails, point `DORIS_FE_IMAGE` and `DORIS_BE_IMAGE` at images whose
`JAVA_OPTS` include `-XX:-UseContainerSupport`.

## End-to-end verification

Two scripts drive the real `etl-replicator` binary against the local stack and
assert on the destination. They are the only coverage for configuration loading,
S3 storage, and the Doris wire protocol.

```bash
scripts/bin/e2e-ducklake.sh
scripts/bin/e2e-doris.sh
```

Both check the initial copy, streamed inserts, updates, and deletes collapsing
to the current source state, a followed column addition, a followed type
widening, and a truncate. The DuckLake script also checks a column rename, a
table rename, and recovery through a resync. The Doris script also checks native
array columns and a changed primary key value. `e2e-doris.sh` needs a `mysql`
client on `PATH`.

## Recovering a divergent table

The replica is disposable: when a table no longer matches its source, the fix is
to copy it again rather than to repair it in place.

```bash
# Detect: compare source row counts against the replica.
CATALOG_CONNINFO='host=localhost port=15434 dbname=ducklake_catalog user=postgres password=changeme' \
  scripts/bin/verify-replica.sh --destination ducklake --pipeline-id 9001
scripts/bin/verify-replica.sh --destination doris --pipeline-id 9002

# Recover: reset table state, then start the replicator to run the copy.
etl-resync --table-id "$(psql "$SOURCE_DSN" -qtAc "select 'public.orders'::regclass::oid")"
```

Replication is asynchronous, so a difference on a table that is being written to
may just be lag; rerun and see whether it persists. `etl-resync` has to run
while the replicator is stopped, because a running replicator keeps table state
in memory.

## Benchmarking

`scripts/bin/bench-replica.sh` drives the replicator through six scenarios and
reports what each clock covers, because the scenarios are not comparable
otherwise.

```bash
DESTINATION=ducklake ROWS=100000 TABLES=4 scripts/bin/bench-replica.sh
DESTINATION=doris ROWS=100000 TABLES=4 scripts/bin/bench-replica.sh
```

| Scenario | What the clock covers |
| --- | --- |
| `catchup` | The replicator is stopped, rows are written to build a backlog, then the clock runs from replicator start until the destination count matches. Includes reconnect and a cold table. |
| `streaming insert` | The replicator is already running; covers the source write and the wait for the destination to match. |
| `warm update` / `warm delete` | Like streaming, but against rows the table already holds. |
| `multi-table insert` | Concurrent writes to several tables. |
| `interleaved i/u/d` | Inserts, updates, and deletes mixed inside one transaction, which is what a real change stream looks like. |

Throughput is `rows * 1000 / elapsed_ms`, floored. Source write time is reported
separately and not subtracted, so a streaming number is end to end. These are
single observations, not percentiles.

Set `LAKE_DATA_PATH` to a local directory to keep object-storage latency out of
the numbers. After each run the script scrapes
`etl_ducklake_batch_stage_duration_seconds` from the replicator's Prometheus
endpoint on port 9000, which splits a batch into `begin`, `upsert`, `delete`,
`update`, `marker`, and `commit`. Reach for that distribution before optimising
anything; the stage totals are what turned several guesses into measurements.

### Local environment traps

These cost hours to rediscover:

- **rustfs beta fails HTTP transfers intermittently** under benchmark load, which
  exhausts batch retries and stops the replicator. It is not a configuration
  difference and not specific to any column type. Point `LAKE_DATA_PATH` at a
  local directory for measurements.
- **A repository on a `/mnt/c` 9p mount can hand cargo a stale copy of a file you
  just edited**, so a build succeeds against old source. The benchmark and check
  scripts `touch` recently modified files first; do the same in any new script.
- **Repeated benchmark runs exhaust the source's replication slots.** The script
  drops inactive slots first; a manual run may need
  `select pg_drop_replication_slot(slot_name) from pg_replication_slots where not active`.
- **Leftover DuckLake catalog rows reject a new attach** when the data path
  differs by as much as a `file://` prefix. Wipe `ducklake%` tables from the
  catalog between runs with a different data path.

### Where the time goes

Measured on one machine with a local data path, comparing 5000 rows against
100000 rows to separate the fixed cost from the per-row cost:

| Path | Per row | Fixed |
| --- | --- | --- |
| Streaming insert | ~0.031 ms | ~1.3 s |
| Warm update / delete | ~0.42 ms | ~1 s |
| Initial copy | ~0.06 ms | ~17 s |

The insert path is in the same range as the reference implementation this fork
was measured against. Two gaps remain, both with a known cause:

**Deletes cost 13x more per row than inserts.** A batch already collapses into a
single delete, so the cost is now the predicate list itself: a delete of 100000
keys builds an expression with 100000 `OR` branches, which cannot use column
statistics to prune. Writing the keys into a staging table and matching with
`DELETE FROM t WHERE EXISTS (SELECT 1 FROM stg s WHERE t.k = s.k)` turns that
into one hash join. This is the largest known win left.

**The initial copy pays about 17 seconds of fixed cost** that is not in the write
path: writing 5000 rows spends 0.33 s in the `upsert` stage. It is spread across
pipeline startup, table creation, and table state transitions, none of which the
current stage timings cover. Extending the timings to the table sync worker is
the prerequisite for reducing it.

Two smaller items: a DuckLake commit measures about 108 ms against roughly 23 ms
for the reference implementation, cause not yet investigated; and
`arrow_column_kinds` returns `None` for UUID, JSON, and JSONB, so one such column
sends a whole table down the row-by-row appender path. Staging already carries
JSON as text, so `Utf8` would match.

## Migrations

Two migration sets live under `crates/etl/migrations/`:

- `source/`: helpers every pipeline needs, such as schema snapshot functions and
  the DDL event trigger. `Pipeline::start()` applies these automatically.
- `postgres_store/`: tables that persist replication state, versioned table
  schemas, and destination metadata. `PostgresStore::new()` applies these
  automatically.

Both write to `etl._sqlx_migrations`, so running them separately requires SQLx's
`--ignore-missing` flag.

```bash
export DATABASE_URL=postgres://USER:PASSWORD@HOST:PORT/DB
cargo x migrate etl
```

Never edit an applied migration file, including its comments. SQLx stores a
SHA-384 checksum of the full contents, so even a comment change breaks existing
databases.

## Running the replicator

Configuration loads in three layers: `configuration/base.yaml`, then
`configuration/{environment}.yaml`, then `APP_`-prefixed environment overrides.
`APP_ENVIRONMENT` selects the environment and defaults to `prod`.

```bash
cd crates/etl-replicator
APP_ENVIRONMENT=dev cargo run --release
```

The Docker image needs both configuration files mounted:

```bash
docker run \
  -v $(pwd)/crates/etl-replicator/configuration/base.yaml:/app/configuration/base.yaml \
  -v $(pwd)/crates/etl-replicator/configuration/dev.yaml:/app/configuration/dev.yaml \
  -e APP_ENVIRONMENT=dev \
  etl-replicator:latest
```

## Tests

Tests run through `cargo-nextest`, which uses one process per test. Integration
tests are consolidated into `tests/main.rs` per crate, so a module is addressed
as `-- module_name::`.

```bash
cargo x nextest run                                  # full sharded suite
cargo nextest run --workspace --all-features --lib   # unit tests only
cargo nextest run -p etl-config --all-features       # one crate
cargo test --doc --workspace --all-features          # doctests
```

Required environment variables for anything that touches Postgres:

| Variable | Required | Description |
| --- | --- | --- |
| `TESTS_DATABASE_HOST` | yes | Postgres host |
| `TESTS_DATABASE_PORT` | yes | Postgres port |
| `TESTS_DATABASE_USERNAME` | yes | Database user |
| `TESTS_DATABASE_PASSWORD` | no | Database password |
| `TESTS_DATABASE_REPLICA_HOST` | no | Defaults to `TESTS_DATABASE_HOST` |
| `TESTS_DATABASE_REPLICA_PORT` | no | Defaults to `TESTS_DATABASE_PORT + 1000` |
| `TESTS_DATABASE_TLS_ENABLED` | no | Require verified TLS when `true` |
| `TESTS_DATABASE_TLS_ROOT_CERT` | no | Defaults to `target/postgres-tls/root.crt` |
| `ETL_DUCKDB_EXTENSION_ROOT` | no | Vendored DuckDB extension root |

Each test creates a database with a UUID-based name and drops it afterwards.

DuckLake tests need the vendored DuckDB extensions:

```bash
cargo x vendor-duckdb
export ETL_DUCKDB_EXTENSION_ROOT="$(pwd)/vendor/duckdb/extensions"
```

Property tests built on `etl::test_utils::property` run until a wall-clock budget
elapses. `PROPERTY_TEST_BUDGET_SECS` sets the budget per property, and
`PROPERTY_TEST_SEED` replays a failing chunk.

For debugging, `ENABLE_TRACING=1` turns on tracing output and `RUST_LOG` scopes
it, for example
`RUST_LOG=etl::replication::apply=debug,etl_destinations::ducklake=debug`.

If test output shows `0 passed; 0 failed; 0 ignored; n filtered out`, treat that
as a failure to run tests and check the filter with `cargo nextest list`.

## Troubleshooting

Verify the containers and the connection:

```bash
docker compose -f scripts/docker/docker-compose.yaml ps
psql "$DATABASE_URL" -c "select 1"
psql "$DATABASE_URL" -c "select * from etl._sqlx_migrations"
```
