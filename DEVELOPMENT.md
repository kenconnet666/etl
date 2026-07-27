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

Two knobs matter enough to be worth naming. `BATCH_MAX_BYTES` and
`BATCH_MAX_FILL_MS` override the pipeline's batch shape, which is how much of a
result turns out to be batch granularity rather than per-row cost. `ROW_SHAPE`
selects between `wide`, six columns including `jsonb` and `timestamptz`, and
`narrow`, three columns of `bigint`, `text`, and `numeric`; the second one exists
to separate per-cell decoding cost from everything else and to compare against
benchmarks built on a narrow row.

```bash
DESTINATION=ducklake ROWS=1000000 ROW_SHAPE=narrow BATCH_MAX_BYTES=67108864 \
  scripts/bin/bench-replica.sh
```

| Scenario | What the clock covers |
| --- | --- |
| `catchup` | The replicator is stopped, rows are written to build a backlog, then the clock runs from replicator start until the destination count matches. Includes reconnect and a cold table. |
| `streaming insert` | The replicator is already running; covers the source write and the wait for the destination to match. |
| `warm update` / `warm delete` | Like streaming, but against rows the table already holds. |
| `multi-table insert` | Concurrent writes to several tables. |
| `interleaved i/u/d` | Inserts, updates, and deletes mixed inside one transaction, which is what a real change stream looks like. |

Throughput is `rows / (source write + destination drain)`, floored. Both parts
belong in the denominator for the streaming and warm scenarios, because the
replicator drains while the source is still writing; the script prints the
drain-only rate alongside it, and that one reads between a tenth and a third
higher depending on how much of the write the drain overlapped. `catchup` is the
exception: the replicator is stopped for the whole source write, so its drain is
the entire measurement. These are single observations, not percentiles.

Set `LAKE_DATA_PATH` to a local directory to keep object-storage latency out of
the numbers. After each run the script scrapes
`etl_ducklake_batch_stage_duration_seconds` from the replicator's Prometheus
endpoint on port 9000, which splits a batch into `begin`, `upsert`, `delete`,
`update`, `marker`, and `commit`. Reach for that distribution before optimising
anything; the stage totals are what turned several guesses into measurements.

### Local environment traps

These cost hours to rediscover:

- **A published Docker port caps replication throughput.** `docker-proxy` copies
  every packet through a userland process, and the same drain measures
  163,159 rows/s over a unix socket, 69,842 over the bridge, and 53,350 through the
  published port. Set `PG_HOST` and `CATALOG_HOST` to the containers' bridge
  addresses, which `docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}'`
  prints, and treat any figure taken through a published port as a lower bound.
- **rustfs beta fails HTTP transfers intermittently** under benchmark load, which
  exhausts batch retries and stops the replicator. It is not a configuration
  difference and not specific to any column type. Point `LAKE_DATA_PATH` at a
  local directory for measurements.
- **A repository on a `/mnt/c` 9p mount can hand cargo a stale copy of a file you
  just edited**, so a build succeeds against old source. The benchmark and check
  scripts `touch` recently modified files first; do the same in any new script.
  Copying the tree into the Linux filesystem avoids the problem altogether and
  builds several times faster.
- **The first run in a fresh environment pays a one-off DuckDB extension
  download** inside the replicator, which lands entirely in the `catchup`
  scenario because it is first. At 100,000 rows that took `catchup` from 10,931
  rows/s with a warm extension cache down to 2,869 on the very first run. Discard
  the first run.
- **Repeated benchmark runs exhaust the source's replication slots.** The script
  drops inactive slots first; a manual run may need
  `select pg_drop_replication_slot(slot_name) from pg_replication_slots where not active`.
- **Leftover DuckLake catalog rows reject a new attach** when the data path
  differs by as much as a `file://` prefix. Wipe `ducklake%` tables from the
  catalog between runs with a different data path.
- **A WSL Docker engine can inherit Docker Desktop's credential helper.** When
  `~/.docker/config.json` names `credsStore: desktop` but
  `docker-credential-desktop.exe` is not on `PATH`, even anonymous image pulls
  fail with `error getting credentials`. Remove the key for that distribution.
- **The Doris frontend image crash-loops on some WSL2 kernels.** Doris 4.1.3
  fails in `CgroupV2Subsystem.getInstance` with `Cannot invoke
  CgroupInfo.getMountPoint() because "anyController" is null`, because the JVM
  finds no usable controller mount even though `/sys/fs/cgroup` is `cgroup2fs`.
  Passing `JAVA_OPTS=-XX:-UseContainerSupport` gets the JVM past that point but
  the frontend still restarts in a loop, so an image whose own `JAVA_OPTS`
  disable container support is the workaround, not an environment override.

### Where the time goes

Measured on the standard configuration with a local data path. Comparing 100,000
rows against 1,000,000 separates the fixed cost from the per-row cost. Both columns
are fitted from the end-to-end totals of the two row counts:

| Path | Per row | Fixed per scenario |
| --- | --- | --- |
| Initial copy | ~0.007 ms | ~8.8 s |
| Warm delete | ~0.010 ms | ~0.5 s |
| Multi-table insert | ~0.015 ms | ~0.7 s |
| Streaming insert | ~0.018 ms | ~0.9 s |
| Warm update | ~0.023 ms | ~0.5 s |
| Interleaved i/u/d | ~0.046 ms | ~0.5 s |

The fixed cost is the batch fill window, connection setup, and the benchmark's
own polling granularity, plus process start and catalog bootstrap for the initial
copy. It dominates at 100,000 rows and fades at 1,000,000, which is why a figure
is only comparable against another at the same row count.

Collapsing a batch by key before writing is what makes the interleaved scenario
tractable at all, on both destinations. Before it, a Doris batch opened a new
Stream Load whenever a key repeated, so a stream that inserts, updates, and
deletes the same keys cost one HTTP round trip per operation switch and the
scenario never finished. The same shape of fix on the DuckLake side took it from
never finishing to 1.6 s at 5,000 rows.

**Staging uses the DuckDB appender throughout**, including for the benchmark's
`jsonb` column: every `prepared_rows_kind` label in a full run reads `appender`,
so the row-by-row SQL fallback is not on this path.

**Decoding is cheap; getting the changes out is not.** Peeling the same
1,000,000-change backlog apart by layer, wide row shape, draining into
`/dev/null`:

| Path from consumer to source | rows/s | Added over the layer above |
| --- | --- | --- |
| `pg_logical_slot_peek_binary_changes` in a SQL backend | 258,732 | 3.9 µs/change, decoding only |
| Unix domain socket | 154,966 | +2.6 µs |
| Host network, TCP to `127.0.0.1` | 142,877 | +0.5 µs |
| TCP over the Docker bridge | 69,842 | +7.4 µs |
| TCP through a published port and `docker-proxy` | 57,763 | +3.0 µs |

Host networking and a unix socket land within 8% of each other, so TCP itself is
not the expensive part — the extra hops are. A published port more than halves the
rate, because `docker-proxy` copies every packet through a userland process.

**Host networking is the standard configuration for any throughput figure here.**
Both Postgres instances run with `--network host` on separate ports, and the bench
script is pointed at them through `PG_HOST`, `PG_PORT`, `CATALOG_HOST`, and
`CATALOG_PORT`:

```bash
docker run -d --name etl-src-host --network host -e POSTGRES_PASSWORD=changeme \
  etl-local-pg18 postgres -c wal_level=logical -c max_replication_slots=10 \
  -c max_wal_senders=10
docker run -d --name etl-cat-host --network host -e POSTGRES_USER=lake_admin \
  -e POSTGRES_PASSWORD=changeme -e POSTGRES_DB=ducklake_catalog \
  etl-local-pg18 postgres -c port=5433

DESTINATION=ducklake ROWS=1000000 TABLES=4 \
  PG_HOST=127.0.0.1 PG_PORT=5432 CATALOG_HOST=127.0.0.1 CATALOG_PORT=5433 \
  LAKE_DATA_PATH=/tmp/lake scripts/bin/bench-replica.sh
```

The walsender is pinned at 0.96 to 0.99 cores throughout every streamed drain, and
`pg_stat_activity` shows it `RUNNING` for 357 of 419 samples with no `ClientWrite`
at all, so it is not blocked on the consumer — it is burning CPU in its own send
path, and that cost scales with the number of hops.

**Where the streaming insert's 18.8 µs per change goes.** On the standard
configuration a streaming insert reaches 53,262 rows/s against a bare drain of
142,877 on the same path, or 37%:

| Layer | µs per change | How it was measured |
| --- | --- | --- |
| Source hands the change over | 7.0 | `pg_recvlogical` drain, host network |
| Destination write | 4.1 | 18.6 s of DuckLake stage sums over 4,500,019 rows |
| Unattributed, our own path | 7.7 | the remainder |

The destination is not the constraint: `upsert` 9.2 s over 212 batches, `delete`
5.7 s over 132, `commit` 3.3 s over 224, and `marker` plus `begin` under 0.5 s
together, which is roughly 244,000 rows/s of capacity. The median batch carries
16,772 rows, so per-transaction cost is already amortised, and `blocking_slot_wait`
plus `pool_checkout_wait` stay under 0.1 s for the whole run. Every
`prepared_rows_kind` label reads `appender`, so the benchmark's `jsonb` column does
not fall back to row-by-row SQL either.

**A second implementation on the same machine, compared at two row counts.** The
`debezium-server-ducklake` project in this fork's lineage runs the same shape of
pipeline — Postgres logical replication into DuckLake with a Postgres catalog, one
reader and one writer — in Java 25. Compared on the narrow row shape both use, host
network, same end-to-end convention:

| Scenario | etl 1M | Java 1M | etl 5M | Java 5M |
| --- | --- | --- | --- | --- |
| Initial copy / catchup | 101,132 | 228,885 | **375,742** | 175,143 |
| Streaming insert | 117,274 | 212,269 | 124,672 | 172,300 |
| Warm update | 94,206 | 169,923 | 93,092 | 101,204 |
| Warm delete | 115,420 | 246,305 | **144,977** | 86,499 |
| Multi-table insert | 144,397 | 204,540 | 134,945 | 149,880 |

At 1,000,000 rows the Java implementation leads everywhere by 1.4x to 2.3x. At
5,000,000 it leads by 1.1x to 1.4x on streaming, warm update, and multi-table, and
loses by 2.1x on the initial copy and 1.7x on warm delete. Our figures barely move
between the two row counts; its degrade by 19% on streaming, 40% on warm update, and
65% on warm delete, and its per-batch lake transaction grows from 151 ms to 550 ms.
So a single-row-count comparison of these two is close to meaningless, and 1,000,000
rows is small enough that fixed cost still decides it.

Neither is near the source. Server-side decoding with no protocol involved reaches
322,165 to 327,119 changes/s on the narrow shape and 260,349 to 260,893 on the wide
one, so at 5,000,000 rows the Java streaming insert is at 53% of that ceiling and
ours at 38%.

**The JVM is not the overhead.** Across its 5,000,000-row run: 102 GC pauses
totalling 178 ms against 185 s of test time, a heap that never exceeded 752 MB,
1.5 GB of peak process memory, and 4 min 22 s of CPU for 185 s of wall time, or
about 1.4 cores. Its all-`VARCHAR` staging means very little object churn per row,
which is why GC stays irrelevant. Any argument for or against that implementation
has to rest on something other than JVM cost.

Two caveats on the comparison. Its catchup restarts only a reader thread while ours
restarts a process, and its catalog shares the source instance. And our benchmark
polls the destination by spawning a fresh `duckdb` CLI that attaches the catalog
each time, roughly 200 ms per check, while its test polls in process — that inflates
our measured drain and our CPU accounting, so the 11 min 19 s of CPU our
5,000,000-row run consumed is not comparable to its 4 min 22 s.

**What moves our own number, measured at 1,000,000 rows.** Two config values and one
row-shape effect:

| Configuration | Streaming insert | Factor |
| --- | --- | --- |
| Defaults, six-column row with `jsonb` and `timestamptz` | 51,198 | — |
| `batch.max_bytes` 8 MiB to 64 MiB | 65,163 | 1.27x |
| Narrow three-column row as well | 117,274 | 1.80x |

The 8 MiB default caps a batch at a median 16,772 rows and produces 223 DuckLake
transactions per run against 68 at 64 MiB. Raising `max_fill_ms` past 500 ms does not
help and slightly hurts, and `RUST_LOG=error` instead of `info` changes nothing
measurable, so neither the fill window nor per-message logging is the cost.

Row shape is the larger factor because every cell arrives as Postgres text and is
parsed into a typed value before it is staged: `serde_json::from_str` builds a
`serde_json::Value` for `jsonb`, `chrono::NaiveDateTime::parse_from_str` handles
`timestamptz`, and `numeric` goes through a decimal parse. The Java implementation
stages every column as `VARCHAR` straight into the DuckDB appender and casts in bulk
inside one `insert ... select cast(...)`, which runs vectorised in C++, so row shape
costs it very little.

Staging as text and letting DuckDB cast is therefore an available design change
rather than a micro-optimisation. It trades our own type mapping and validation for
DuckDB's, so it is a real decision, and it should be measured on the wide shape
where the cost actually is.

Two protocol options reduce the bytes and messages the send path has to move, and
both are off today: `raw.rs` asks for `("proto_version" '1', "publication_names"
..., "messages" 'true')` and nothing else.

| Slot options | rows/s | walsender |
| --- | --- | --- |
| `proto_version 1`, text tuples (current) | 53,513 | 1.00 cores, 18.6 µs/row |
| `proto_version 1`, `binary 'true'` | 59,719 | 1.00 cores, 16.7 µs/row |

For one large mixed transaction of 1,250,000 changes, in-progress streaming is the
larger lever, because the default `logical_decoding_work_mem` of 64 MB cannot hold
the transaction and the reorder buffer spills:

| Slot options | changes/s | `pg_stat_replication_slots` |
| --- | --- | --- |
| `streaming` off (current) | 50,709 | `spill_txns=1`, `spill_bytes=216 MB` |
| `streaming 'on'` | 66,988 | no spill, `stream_txns=1` |

Both of those, and the sharding numbers below, were measured through a published
port, so their absolute values are capped and their relative sizes need redoing on
the standard configuration.

Sharding across slots scales sublinearly. Draining 1,000,000 rows spread over four
tables: 58,779 rows/s through one slot, 100,010 through two, 130,856 through four.
Four slots buy 2.2x rather than 4x, because every walsender still reads all of the
WAL and only the publication filter and the output functions are saved.

To reproduce the layer breakdown, create the slots before writing the rows so each
drain covers exactly those changes, and stop at a known LSN rather than on a
timeout:

```bash
# Server-side decoding with no protocol at all. pgoutput needs the binary variant.
psql "$SOURCE_DSN" -qtAc "select count(*) from pg_logical_slot_peek_binary_changes(
  'ceiling', null, null, 'proto_version', '1', 'publication_names', 'ceiling_pub')"

# Unix socket. Run the source with its socket directory bind mounted, then use a
# libpq keyword string rather than a URL.
pg_recvlogical -d "host=/run/pg-sock port=5432 user=postgres dbname=postgres" \
  --slot=ceiling --start --no-loop \
  -o proto_version=1 -o publication_names=ceiling_pub -E "$END_LSN" -f /dev/null
```

Do not sample the walsender's CPU with a `psql` call in a tight loop while
measuring throughput. Spawning a process every 50 ms costs enough to slow the
drain: the same host-network loopback path measured 81,893 rows/s under a sampler
and about 140,000 rows/s without one. Sample CPU and measure throughput in separate
runs.

Going materially faster still means moving the bottleneck off a single walsender,
which means several replication slots decoding in parallel, sharded by table or
schema. That is a deployment topology rather than a code change, and it costs
roughly N times the source-side WAL decoding because every walsender decodes all
of the WAL before filtering by publication.

**The initial copy's fixed cost is now attributed.** It is about 8.4 s, and the
largest identified component is the per-table write slot: `copy_prepare` spends
24.0 s in `table_write_slot` across 52 acquisitions in a 1,000,000-row run, which
overlaps with real work but bounds how much of the copy can proceed in parallel.
`ensure_table` adds 2.6 s over the same 52 calls. The remainder is process start
and DuckDB extension load, which is why a cold extension cache inflates the
scenario so badly. At 1,000,000 rows the whole fixed cost is amortised and the
copy reaches 61,713 rows/s.

`arrow_column_kinds` returns `None` for UUID, JSON, and JSONB. That no longer
forces the row-by-row path, but it does keep those columns off the Arrow fast
path. Staging already carries JSON as text, so `Utf8` would match.

### Next steps, in order

Ranked by measured payoff against the work each needs.

1. **Reconsider the 8 MiB `batch.max_bytes` default.** Raising it to 64 MiB is worth
   1.27x on a wide row and 1.25x on a narrow one, for one config value. The
   defaults exist to bound memory, so the question is whether 8 MiB is the right
   bound at 30 GB of RAM rather than whether larger batches help. Raising
   `max_fill_ms` past 500 ms does not help and slightly hurts.
2. **Evaluate staging as text and casting in DuckDB.** That is the remaining 1.72x
   against the Java implementation and the reason a wide row costs 1.80x. Measure it
   on the wide shape first, on one column type at a time, because it trades our own
   type mapping for DuckDB's and the two do not agree on every edge case.
3. **Instrument the path between the socket and the destination write** so items 1
   and 2 stop being inferred from end-to-end numbers. Split `handle_stream_message`
   into protocol parse, tuple decode, row and cell construction, and batch push with
   byte accounting. Check whether `MAX_DRAINED_MESSAGES_PER_ITERATION` has become
   the new limit and whether the socket is read in large chunks. Note that
   `RUST_LOG=error` instead of `info` changes nothing measurable, so per-message
   logging is not the cost.
4. **Keep the transport out of the way in deployment.** Host networking or a unix
   socket moves about 143,000 to 155,000 rows/s where a published port moves
   57,763. It costs nothing.
5. **Ask for `streaming 'on'` at `proto_version` 2 or above.** Measured +32% on one
   large mixed transaction and it removes a 216 MB reorder-buffer spill, which is
   server-side and therefore independent of transport. Needs `stream_start`,
   `stream_stop`, `stream_commit`, and `stream_abort` handling in
   `crates/etl/src/postgres/client/raw.rs` and the apply loop. Note that applying
   changes before their commit arrives turns an aborted transaction into permanent
   divergence rather than temporary lag, so buffering per transaction on our side
   keeps the source-side win without that risk.
6. **Ask for `binary 'true'`**, worth +11.6% when measured through a published port.
   It removes text output-function work from the walsender, and it would remove our
   own text parsing too, which overlaps with item 2.
7. **Shard across several replication slots** when one slot is genuinely saturated.
   At 117,274 rows/s on a narrow row against a 164,284 rows/s single-slot drain it
   is getting closer, but items 1 to 3 come first.
8. **Reduce `table_write_slot` serialisation in the copy path**, which is the
   largest identified component of the initial copy's fixed cost at 26.2 s over 52
   acquisitions in a 1,000,000-row run.
9. **Widen the Arrow column coverage** so JSON and UUID columns stop falling off
   the fast path.

### Known open issues

- `cargo clippy -p etl-destinations --features ducklake --no-default-features`
  fails with `cannot find signal in tokio`. It predates this work: the
  destination uses `tokio::signal` but that feature only arrives through another
  crate's default features. `--all-features` is clean, and so is the workspace
  build, so it only shows up in that one narrow invocation.
- rustfs beta is not usable for load testing; see the traps above. The e2e scripts
  still run against it and pass, because their volumes are small and the
  replicator retries.
- The Doris destination has no current measurement, because the official Doris
  4.1.3 frontend image crash-loops on this WSL2 kernel; see the traps above.
- The benchmark's `catchup` scenario is dominated by fixed cost below roughly
  500,000 rows, so treat smaller figures as latency rather than throughput.

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
