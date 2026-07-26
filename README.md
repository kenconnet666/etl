# ETL

A Rust service that turns a PostgreSQL 18 primary into a near-real-time
analytical replica.

This is a fork of [supabase/etl](https://github.com/supabase/etl) trimmed to one
job: keep an analytical store that mirrors the current state of a Postgres
source. It is not an audit log, and it is not a general-purpose replication
framework.

```mermaid
flowchart LR
    Postgres["Postgres 18 primary<br/>logical replication"] --> ETL["etl-replicator<br/>copy + stream"]
    ETL --> DuckLake["DuckLake<br/>Postgres catalog + S3 storage"]
    ETL --> Doris["Apache Doris<br/>unique key + merge-on-write"]
```

## What it guarantees

| Property | Behaviour |
| --- | --- |
| Consistency | Eventual. Replication is at-least-once, and every destination write is idempotent. |
| Data shape | Current state of the source, not an event log. Updates overwrite and deletes remove. |
| Schema changes | Added, dropped, renamed, and retyped columns are followed, as are table renames. |
| Types | Strongly typed wherever the destination can express the Postgres type, with text as a lossless fallback. |
| Tables without a key | A source table with no primary key and no `replica identity full` degrades to an append-only log, because the source never sends a key image. |

## Destinations

| Feature | Destination | Notes |
| --- | --- | --- |
| `ducklake` | DuckLake | Postgres catalog plus local or S3-compatible storage. Replay epochs, applied-batch markers, and streaming progress make retries idempotent. |
| `doris` | Apache Doris | Unique-key tables with merge-on-write. Stream Load labels derived from the source position make retries idempotent. |

## Requirements

- PostgreSQL 18 source with `wal_level = logical`. Postgres 17 is also covered by CI.
- A Postgres instance for the DuckLake catalog, separate from the source.
- S3-compatible object storage for DuckLake data files. The development stack uses
  [rustfs](https://rustfs.com); MinIO works the same way.
- A Doris 4.x cluster when the Doris destination is enabled.

## Running

The replicator loads its configuration from `configuration/{environment}.yaml`
inside `crates/etl-replicator`, with `APP_`-prefixed environment overrides.

```bash
cd crates/etl-replicator
APP_ENVIRONMENT=dev cargo run --release
```

See [DEVELOPMENT.md](DEVELOPMENT.md) for the local stack, migrations, and tests.

## Performance

Measured on one developer machine (WSL2 Debian, Postgres 18 source, Postgres 18
DuckLake catalog, local data path so object-storage latency stays out of the
numbers). Throughput is `rows * 1000 / elapsed_ms`; these are single observations
rather than percentiles, and the source write time is reported alongside rather
than subtracted, so each figure is end to end.

| Scenario | Rows | Source write | To the replica | Rows/s |
| --- | --- | --- | --- | --- |
| Initial copy, catching up a backlog | 100,000 | 835 ms | 19,381 ms | 5,159 |
| Streaming insert | 100,000 | 913 ms | 3,758 ms | 26,609 |
| Streaming insert, 4 tables | 75,000 | 906 ms | 2,831 ms | 26,492 |
| Interleaved insert/update/delete | 50,000 | 2,354 ms | 3,326 ms | 15,033 |
| Warm update | 100,000 | 881 ms | 3,884 ms | 25,746 |
| Warm delete | 50,000 | 227 ms | 2,035 ms | 24,570 |

Streaming throughput sits in the same range across inserts, updates, and
deletes, because a batch collapses by key into one delete matched through staged
keys plus one insert, whatever mix of operations it contains. The initial copy is
the outlier: its own table sync finishes in about 4.4 seconds, so most of the
figure above is startup work outside the write path that has not been attributed
yet.

`scripts/bin/bench-replica.sh` reproduces these numbers, and
[DEVELOPMENT.md](DEVELOPMENT.md) documents the measurement scope, the known gaps,
and the local environment traps that distort results.

## Recovering from divergence

The replica holds no authority over the data, so recovery is a fresh copy rather
than a repair. `scripts/bin/verify-replica.sh` compares source row counts against
the replica, and `etl-resync` resets table state so the replicator drops the
destination table and copies it again.

```bash
scripts/bin/verify-replica.sh --destination doris
etl-resync --table-id "$(psql "$SOURCE_DSN" -qtAc "select 'public.orders'::regclass::oid")"
```

Run `etl-resync` while the replicator is stopped, then start it to perform the
copy.

## License

Apache-2.0. See `LICENSE` for details.
