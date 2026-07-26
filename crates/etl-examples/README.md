# ETL Examples

Runnable pipelines that replicate a Postgres source into one destination. They
exist to exercise a destination end to end without wiring up the replicator
binary and its configuration files.

| Example | Binary | Feature | Destination |
| --- | --- | --- | --- |
| DuckLake | `ducklake` | `ducklake` | DuckLake over a Postgres catalog and local or S3 storage |

## Running

The task runner fills in the source connection from the `TESTS_DATABASE_*`
environment variables:

```bash
cargo x seed                 # create example tables and data
cargo x example ducklake     # run the pipeline
```

Extra flags are passed through:

```bash
cargo x example ducklake --db-name mydb --publication my_pub
```

Without the task runner:

```bash
cargo run --bin ducklake -p etl-examples --features ducklake -- [flags]
```

Every example accepts `--help`.

## DuckLake

DuckLake needs a Postgres catalog and a data path. The catalog must be a
different database from the replication source.

```bash
cargo run --bin ducklake -p etl-examples --features ducklake -- \
  --db-host localhost --db-port 5430 --db-username postgres --db-password postgres \
  --db-name etl_testdata --publication seed_pub \
  --ducklake-catalog-url postgres://postgres:postgres@localhost:5430/ducklake_catalog \
  --ducklake-data-path file:///tmp/ducklake-data
```

For S3-compatible storage, pass an `s3://` data path together with the access
key, secret key, region, endpoint, and URL style flags.

DuckLake loads the `ducklake` and `postgres_scanner` DuckDB extensions. Vendor
them once and point the destination at them:

```bash
cargo x vendor-duckdb
export ETL_DUCKDB_EXTENSION_ROOT="$(pwd)/vendor/duckdb/extensions"
```

Without `ETL_DUCKDB_EXTENSION_ROOT`, DuckDB installs the extensions from its own
repository at runtime, which requires network access.
