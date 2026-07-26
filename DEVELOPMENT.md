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

Measured on one machine with a local data path. Comparing 100,000 rows against
1,000,000 separates the fixed cost from the per-row cost:

| Path | Per row | Fixed per scenario |
| --- | --- | --- |
| Streaming insert | ~0.021 ms | ~1.5 s |
| Warm update | ~0.033 ms | ~1 s |
| Warm delete | ~0.018 ms | ~1 s |
| Initial copy | ~0.016 ms | ~18 s |

The fixed cost is the batch fill window, connection setup, and the benchmark's
own polling granularity. It dominates at 100,000 rows and fades at 1,000,000,
which is why a figure is only comparable against another at the same row count.

**Throughput is bounded by the source, not by this code.** Draining the same
1,000,000 rows from an equivalent slot with `pg_recvlogical` straight into
`/dev/null` — no row decoding, no destination — takes 25.9 s, or 38,630 rows/s.
The full chain takes 26.6 s, or 37,601 rows/s, so it captures 97% of what the
walsender can deliver on this machine.

Three earlier measurements line up behind that. The destination's `upsert` stage
writes rows at about 5.8 microseconds each and `delete` matches keys at 5.2, which
is roughly 170,000 rows/s of capacity. `blocking_slot_wait` totals 2.8 ms across
531 acquisitions and `pool_checkout_wait` 0.59 s, so the destination never queues
for a resource. And the apply loop spends 94 s awaiting messages against 38 s
handling them — a ratio that did not move when draining buffered messages cut the
loop's iteration count 18-fold, from 4,500,525 to 241,001.

So the remaining 3% is the whole optimisation budget on this path. Going
materially faster needs the bottleneck moved off a single walsender, which means
several replication slots decoding in parallel, sharded by table or schema. That
is a deployment topology rather than a code change, and it costs roughly N times
the source-side WAL decoding because every walsender decodes all of the WAL before
filtering by publication.

**The initial copy carries about 18 seconds that is still unattributed.** Its own
table sync takes 4.4 s — the state log goes `init` to `data_sync` to `sync_done`
in that time — while the benchmark measures 19 s at 100,000 rows. Ruled out so
far: the number of tables (a single table measures the same), the batch fill
window (a longer one changes nothing here), per-table write slot contention
(20 batches wait 8.3 s in total but that overlaps with work), and serial
connection pool warm-up (now parallel, with no effect). Worth checking next:
process startup before the pipeline runs, source-side snapshot export and `COPY`
throughput, and whether the benchmark's polling still inflates it. At 1,000,000
rows this cost is amortised and the copy reaches 29,670 rows/s.

`arrow_column_kinds` also returns `None` for UUID, JSON, and JSONB, so one such
column sends a whole table down the row-by-row appender path. Staging already
carries JSON as text, so `Utf8` would match.

### Next steps, in order

1. **Profile decode and transport in `etl`.** That is where the remaining
   headroom is: the destination has roughly a 4x margin over the current
   end-to-end rate and spends its time waiting for events. Add stage timings
   around event decoding and the apply loop before changing anything, the same
   way the destination timings turned three wrong guesses into one correct fix.
2. **Attribute the initial copy's fixed cost**, which is about 18 s regardless of
   row count. It amortises at 1,000,000 rows but dominates smaller tables. See
   the list above for what has already been ruled out.
3. **Widen the Arrow column coverage** so a JSON or UUID column stops disabling
   it for a whole table, and look at the commit stage if a profile shows it
   matters.

### Known open issues

- `cargo clippy -p etl-destinations --features ducklake --no-default-features`
  fails with `cannot find signal in tokio`. It predates this work: the
  destination uses `tokio::signal` but that feature only arrives through another
  crate's default features. `--all-features` is clean, and so is the workspace
  build, so it only shows up in that one narrow invocation.
- rustfs beta is not usable for load testing; see the traps above. The e2e scripts
  still run against it and pass, because their volumes are small and the
  replicator retries.
- The benchmark's `catchup` scenario reports a throughput figure that is mostly
  unattributed fixed cost, so treat it as a latency measurement until item 1
  above is done.

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
