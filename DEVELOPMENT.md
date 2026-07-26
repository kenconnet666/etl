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

Copy `.docker/local/.env.example` to `.docker/local/.env` to override the
generated passwords.

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
