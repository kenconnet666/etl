//! Requests a fresh copy of replicated tables.
//!
//! The replica mirrors the current state of its source, so recovering from
//! divergence means copying a table again rather than repairing it in place.
//! This binary resets ETL table state to `init`; the replicator then drops the
//! destination object and copies the table from scratch.
//!
//! Run it while the replicator is stopped. A running replicator keeps table
//! state in memory and would not observe the reset.
//!
//! Durable apply-worker progress is left alone, because it is shared by every
//! table in the pipeline and the reset tables catch up through their own table
//! sync workers.

use std::{collections::BTreeSet, env, error::Error, process::ExitCode};

use etl_config::{load_config, shared::ReplicatorConfig};
use etl_postgres::{
    schema::TableId,
    source::connect_to_source_database,
    store::table_state::{get_table_state_rows, reset_table_state},
};
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

type ResyncResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// Tables to copy again.
enum Request {
    /// Every table the pipeline currently tracks.
    AllTables,
    /// Only the listed source tables.
    Tables(BTreeSet<TableId>),
}

/// Runs the resync binary.
fn main() -> ExitCode {
    match try_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("resync failed: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Parses arguments, loads configuration, and applies the reset.
fn try_main() -> ResyncResult<()> {
    init_stdout_tracing();

    let request = parse_request(env::args().skip(1))?;
    let config = load_config::<ReplicatorConfig>()?;
    config.validate()?;

    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run(config, request))
}

/// Parses the requested tables from the command line.
fn parse_request(args: impl Iterator<Item = String>) -> ResyncResult<Request> {
    let mut all_tables = false;
    let mut table_ids = BTreeSet::new();
    let mut args = args;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--all" => all_tables = true,
            "--table-id" => {
                let value = args.next().ok_or("--table-id needs a value")?;
                let table_id: u32 = value.parse().map_err(|_| {
                    format!("--table-id expects a Postgres table OID, got '{value}'")
                })?;
                table_ids.insert(TableId::new(table_id));
            }
            "--help" | "-h" => return Err(usage().into()),
            other => return Err(format!("unexpected argument '{other}'\n\n{}", usage()).into()),
        }
    }

    match (all_tables, table_ids.is_empty()) {
        (true, true) => Ok(Request::AllTables),
        (false, false) => Ok(Request::Tables(table_ids)),
        (true, false) => Err("--all cannot be combined with --table-id".into()),
        (false, true) => Err(usage().into()),
    }
}

/// Returns the command-line usage text.
fn usage() -> String {
    "usage: etl-resync (--all | --table-id <oid> [--table-id <oid>]...)\n\nResets ETL table state \
     so the replicator copies the tables again. Run it while the replicator is stopped, then start \
     the replicator to perform the copy. Find a table OID with: select \
     'schema.table'::regclass::oid;"
        .to_owned()
}

/// Applies the reset against the ETL state store.
async fn run(config: ReplicatorConfig, request: Request) -> ResyncResult<()> {
    let pipeline_id = config.pipeline.id as i64;
    let pool = connect_to_source_database(&config.pipeline.pg_connection, 1, 1, None).await?;

    let tracked: BTreeSet<TableId> = get_table_state_rows(&pool, pipeline_id)
        .await?
        .into_iter()
        .map(|row| TableId::new(row.table_id.0))
        .collect();
    let requested = match request {
        Request::AllTables => tracked.clone(),
        Request::Tables(table_ids) => table_ids,
    };

    let mut connection = pool.acquire().await?;
    let mut reset_count = 0;
    for table_id in requested {
        if !tracked.contains(&table_id) {
            warn!(pipeline_id, %table_id, "skipping a table the pipeline does not track");
            continue;
        }

        reset_table_state(&mut connection, pipeline_id, table_id).await?;
        reset_count += 1;
        info!(pipeline_id, %table_id, "reset table for a fresh copy");
    }

    if reset_count == 0 {
        return Err("no tracked table matched the request".into());
    }

    info!(pipeline_id, reset_count, "start the replicator to run the copy");

    Ok(())
}

/// Initializes plain stdout tracing.
fn init_stdout_tracing() {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn"));
    tracing_subscriber::registry().with(env_filter).with(tracing_subscriber::fmt::layer()).init();
}
