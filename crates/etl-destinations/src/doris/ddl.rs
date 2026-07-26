//! Doris DDL client that speaks the MySQL protocol.
//!
//! Every statement goes through the text protocol. Doris is MySQL-protocol
//! compatible but does not implement the binary prepared-statement protocol
//! fully: a `PREPARE` answers with a short `PrepareOk` packet that a strict
//! client rejects. Values are therefore quoted into the statement instead of
//! bound.

use std::time::Duration;

use etl::{
    error::{ErrorKind, EtlResult},
    etl_error,
};
use sqlx::{AssertSqlSafe, MySqlPool, Row, mysql::MySqlPoolOptions, raw_sql};
use tracing::{debug, info};

use crate::doris::{DorisTableName, config::DorisConfig, quote_identifier, quote_literal};

/// Interval between schema-change status polls.
const SCHEMA_CHANGE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Executes DDL against a Doris frontend.
#[derive(Clone)]
pub(super) struct DorisDdlClient {
    pool: MySqlPool,
    schema_change_timeout: Duration,
}

impl DorisDdlClient {
    /// Connects to the Doris frontend over the MySQL protocol.
    ///
    /// The connection has no default database because the destination creates
    /// it on first use, so every statement names its database.
    pub async fn connect(config: &DorisConfig) -> EtlResult<Self> {
        let pool =
            MySqlPoolOptions::new().max_connections(1).connect(&config.mysql_url()).await.map_err(
                |source| {
                    etl_error!(
                        ErrorKind::DestinationConnectionFailed,
                        "Doris DDL connection failed",
                        format!("host={} port={}", config.fe_mysql_host, config.fe_mysql_port),
                        source: source
                    )
                },
            )?;

        Ok(Self {
            pool,
            schema_change_timeout: Duration::from_secs(config.schema_change_timeout_secs),
        })
    }

    /// Creates the target database when it does not exist yet.
    pub async fn ensure_database(&self, database: &str) -> EtlResult<()> {
        let sql = format!("create database if not exists {}", quote_identifier(database));
        self.execute(&sql, "Doris create database failed").await
    }

    /// Executes one statement that Doris applies synchronously.
    pub async fn execute(&self, sql: &str, context: &'static str) -> EtlResult<()> {
        debug!(sql, "executing doris ddl");
        raw_sql(AssertSqlSafe(sql.to_owned()))
            .execute(&self.pool)
            .await
            .map_err(|source| etl_error!(ErrorKind::DestinationError, context, source: source))?;

        Ok(())
    }

    /// Executes a statement Doris may apply as a background schema-change job,
    /// then waits for that job to finish.
    ///
    /// Adding, dropping, and renaming a value column are lightweight, but a
    /// type change is queued, and loading against a half-migrated table
    /// fails.
    pub async fn execute_async_schema_change(
        &self,
        sql: &str,
        table_name: &DorisTableName,
    ) -> EtlResult<()> {
        debug!(sql, table = %table_name, "executing doris schema change");
        raw_sql(AssertSqlSafe(sql.to_owned())).execute(&self.pool).await.map_err(|source| {
            etl_error!(
                ErrorKind::DestinationError,
                "Doris schema change submission failed",
                format!("table={table_name}"),
                source: source
            )
        })?;

        self.wait_for_schema_change(table_name).await
    }

    /// Returns the current Doris column names for a table.
    pub async fn column_names(&self, table_name: &DorisTableName) -> EtlResult<Vec<String>> {
        let sql = format!(
            "select column_name from information_schema.columns where table_schema = {} and \
             table_name = {} order by ordinal_position",
            quote_literal(table_name.database()),
            quote_literal(table_name.table())
        );
        let rows = raw_sql(AssertSqlSafe(sql)).fetch_all(&self.pool).await.map_err(|source| {
            etl_error!(
                ErrorKind::DestinationError,
                "Doris column lookup failed",
                format!("table={table_name}"),
                source: source
            )
        })?;

        let mut column_names = Vec::with_capacity(rows.len());
        for row in rows {
            let column_name: String = row.try_get(0).map_err(|source| {
                etl_error!(
                    ErrorKind::DestinationError,
                    "Doris column lookup returned an unexpected shape",
                    format!("table={table_name}"),
                    source: source
                )
            })?;
            column_names.push(column_name);
        }

        Ok(column_names)
    }

    /// Polls the schema-change job list until the table has none running.
    async fn wait_for_schema_change(&self, table_name: &DorisTableName) -> EtlResult<()> {
        let sql = format!(
            "show alter table column from {} where TableName = {} order by JobId desc limit 1",
            quote_identifier(table_name.database()),
            quote_literal(table_name.table())
        );
        let deadline = tokio::time::Instant::now() + self.schema_change_timeout;

        loop {
            let row = raw_sql(AssertSqlSafe(sql.clone()))
                .fetch_optional(&self.pool)
                .await
                .map_err(|source| {
                    etl_error!(
                        ErrorKind::DestinationError,
                        "Doris schema change status query failed",
                        format!("table={table_name}"),
                        source: source
                    )
                })?;

            let Some(row) = row else {
                debug!(table = %table_name, "no doris schema change job found");
                return Ok(());
            };

            let state: String = row.try_get("State").unwrap_or_default();
            match state.as_str() {
                "FINISHED" => {
                    info!(table = %table_name, "doris schema change completed");
                    return Ok(());
                }
                "CANCELLED" => {
                    let message: String = row.try_get("Msg").unwrap_or_default();
                    return Err(etl_error!(
                        ErrorKind::DestinationError,
                        "Doris schema change was cancelled",
                        format!("table={table_name} message={message}")
                    ));
                }
                _ => {}
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(etl_error!(
                    ErrorKind::DestinationError,
                    "Doris schema change did not finish in time",
                    format!(
                        "table={table_name} state={state} timeout_secs={}",
                        self.schema_change_timeout.as_secs()
                    )
                ));
            }

            debug!(table = %table_name, %state, "waiting for the doris schema change");
            tokio::time::sleep(SCHEMA_CHANGE_POLL_INTERVAL).await;
        }
    }

    /// Closes the connection pool.
    pub async fn shutdown(&self) {
        self.pool.close().await;
    }
}
