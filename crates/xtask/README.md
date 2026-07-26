# xtask

Workspace automation, reachable through the `cargo x` alias defined in
`.cargo/config.toml`.

```bash
cargo x --help
```

| Command | Purpose |
| --- | --- |
| `fmt` | Format with the pinned nightly rustfmt. `--check` verifies instead. |
| `check` | Pre-PR gate: fmt, manifest sort, clippy. |
| `fix` | Auto-fix: `clippy --fix`, fmt, manifest sort. |
| `msrv` | Verify the MSRV is consistent across manifests and the toolchain file. |
| `postgres start` | Start the sharded test Postgres clusters with TLS. |
| `migrate` | Run the source and Postgres store migrations. |
| `seed` | Seed a database with example tables and data. |
| `example <name>` | Run a destination example, for example `cargo x example ducklake`. |
| `pg-fill-table` | Fill one table to a target size with parallel `COPY` workers. |
| `test` | Run tests through nextest for the current shell environment. |
| `nextest run` | Run the full suite sharded across the test Postgres clusters. |
| `vendor-duckdb` | Download the pinned DuckDB extensions into `vendor/`. |

The task runner builds into `target/xtask` so alternating between `cargo x` and
`cargo build` does not invalidate shared dependencies whose unified feature sets
differ between the two graphs.
