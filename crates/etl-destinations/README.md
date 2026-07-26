# `etl-destinations`

Destination implementations for the ETL system. Both destinations keep the
current state of the source rather than an event log.

Enable the destination modules you need with crate features:

| Feature | Destination |
| --- | --- |
| `ducklake` | DuckLake over a Postgres catalog and local or S3-compatible storage |
| `doris` | Apache Doris unique-key tables with merge-on-write |

## DuckLake

Source schema changes are followed: added, dropped, renamed, and retyped columns
plus table renames. A type change is applied by rewriting the column, because
DuckLake rejects an in-place type change. Column nullability is not mirrored; the
replica reflects source data, not source constraints.

Idempotency comes from three ETL-owned records keyed by the destination table:
replay epochs in the catalog, applied-batch markers, and streaming progress
watermarks. A retried batch is recognized and skipped.

External maintenance is configured at runtime with `maintenance_mode`, which is
either `disabled` (the default) or `postgres`. Postgres coordination reuses the
DuckLake catalog connection and keeps its state in the `etl` schema.

## Doris

Tables use the unique-key model with merge-on-write so repeated loads of the same
key converge on the latest source row. Rows are loaded through Stream Load with a
label derived from the source position, which makes a retried load a no-op.
Deletes are sent as rows carrying `__DORIS_DELETE_SIGN__`. DDL runs over the
MySQL protocol, and a type change is awaited because Doris applies it as a
background job.

Two protocol details are worth knowing. The frontend answers Stream Load with a
307 redirect to a backend, and an HTTP client that follows redirects
automatically drops the `Authorization` header across hosts, so the client
follows the redirect itself and re-sends credentials. DDL statements also use the
MySQL text protocol rather than bind parameters, because Doris answers a
`PREPARE` with a packet a strict client rejects.

Set `replication_num` to `1` for a single-backend cluster; Doris otherwise
defaults to three replicas and refuses to create the table.

## Tables without a key

A source table with no primary key and no `replica identity full` never sends a
key image, so neither destination can match existing rows. Such a table degrades
to an append-only log: a full update appends the new row, and a partial update or
delete is skipped with a warning. In Doris the table also gains an ETL-owned
`_etl_row_id` key column, because a unique-key table must have one.

## Verification

`cargo nextest run -p etl-destinations --all-features` covers the SQL builders,
type mapping, encoding, and the DuckLake pipeline against a real catalog.
`scripts/bin/e2e-ducklake.sh` and `scripts/bin/e2e-doris.sh` drive the replicator
binary against the local stack; see [DEVELOPMENT.md](../../DEVELOPMENT.md).
