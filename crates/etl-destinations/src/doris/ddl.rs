//! MySQL protocol DDL client for Apache Doris.

use std::time::Duration;

use etl::{
    error::{ErrorKind, EtlResult},
    etl_error,
};
use sqlx::{AssertSqlSafe, MySqlPool, Row, mysql::MySqlPoolOptions};
use tracing::{debug, info};

use crate::doris::{DorisTableName, config::DorisConfig};

/// Poll interval for checking schema change status.
const SCHEMA_CHANGE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Doris DDL client over the MySQL protocol.
#[derive(Clone)]
pub(super) struct DorisDdlClient {
    pool: MySqlPool,
    schema_change_timeout: Duration,
}

impl DorisDdlClient {
    /// Connects to the Doris FE MySQL interface.
    pub async fn connect(config: &DorisConfig) -> EtlResult<Self> {
        let pool = MySqlPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&config.mysql_url())
            .await
            .map_err(|source| {
                etl_error!(
                    ErrorKind::DestinationError,
                    "Doris DDL connection failed",
                    format!("host={}, port={}", config.fe_mysql_host, config.fe_mysql_port),
                    source: source
                )
            })?;

        Ok(Self {
            pool,
            schema_change_timeout: Duration::from_secs(config.schema_change_timeout_secs),
        })
    }

    /// Creates the target database when it does not exist yet.
    pub async fn ensure_database(&self, database: &str) -> EtlResult<()> {
        let sql = format!("CREATE DATABASE IF NOT EXISTS `{}`", database.replace('`', "``"));
        self.execute(&sql, "Doris create database failed").await
    }

    /// Returns the current Doris column names for a table.
    pub async fn column_names(&self, table_name: &DorisTableName) -> EtlResult<Vec<String>> {
        let sql = "SELECT column_name FROM information_schema.columns WHERE table_schema = ? AND \
                   table_name = ? ORDER BY ordinal_position";
        let rows = sqlx::query(sql)
            .bind(table_name.database())
            .bind(table_name.table())
            .fetch_all(&self.pool)
            .await
            .map_err(|source| {
                etl_error!(
                    ErrorKind::DestinationError,
                    "Doris column lookup failed",
                    format!("table={table_name}"),
                    source: source
                )
            })?;

        let mut column_names = Vec::with_capacity(rows.len());
        for row in rows {
            let column_name: String = row.try_get("column_name").map_err(|source| {
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

    /// Executes a DDL statement synchronously.
    pub async fn execute(&self, sql: &str, context: &'static str) -> EtlResult<()> {
        debug!(sql, "executing doris DDL");
        sqlx::query(AssertSqlSafe(sql.to_owned()))
            .execute(&self.pool)
            .await
            .map_err(|source| etl_error!(ErrorKind::DestinationError, context, source: source))?;
        Ok(())
    }

    /// Executes an async schema change and polls until completion.
    pub async fn execute_async_schema_change(&self, sql: &str, table_name: &str) -> EtlResult<()> {
        debug!(sql, table_name, "executing async doris schema change");
        sqlx::query(AssertSqlSafe(sql.to_owned())).execute(&self.pool).await.map_err(|source| {
            etl_error!(
                ErrorKind::DestinationError,
                "Doris async schema change submission failed",
                format!("table={table_name}"),
                source: source
            )
        })?;

        self.poll_schema_change(table_name).await
    }

    /// Polls `SHOW ALTER TABLE COLUMN` until finished or timeout.
    async fn poll_schema_change(&self, table_name: &str) -> EtlResult<()> {
        let deadline = tokio::time::Instant::now() + self.schema_change_timeout;
        let show_sql = format!(
            "SHOW ALTER TABLE COLUMN WHERE TableName = '{table_name}' ORDER BY JobId DESC LIMIT 1"
        );

        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(etl_error!(
                    ErrorKind::DestinationError,
                    "Doris schema change timed out",
                    format!(
                        "table={table_name}, timeout={}s",
                        self.schema_change_timeout.as_secs()
                    )
                ));
            }

            tokio::time::sleep(SCHEMA_CHANGE_POLL_INTERVAL).await;

            let row = sqlx::query(AssertSqlSafe(show_sql.clone()))
                .fetch_optional(&self.pool)
                .await
                .map_err(|source| {
                    etl_error!(
                        ErrorKind::DestinationError,
                        "Failed to poll Doris schema change status",
                        format!("table={table_name}"),
                        source: source
                    )
                })?;

            let Some(row) = row else {
                debug!(table_name, "no schema change job found, assuming completed");
                return Ok(());
            };

            let state: String = row.try_get("State").unwrap_or_default();
            debug!(table_name, %state, "schema change status");

            match state.as_str() {
                "FINISHED" => {
                    info!(table_name, "doris schema change completed");
                    return Ok(());
                }
                "CANCELLED" => {
                    let msg: String = row.try_get("Msg").unwrap_or_default();
                    return Err(etl_error!(
                        ErrorKind::DestinationError,
                        "Doris schema change was cancelled",
                        format!("table={table_name}, msg={msg}")
                    ));
                }
                _ => continue,
            }
        }
    }

    /// Closes the connection pool.
    pub async fn shutdown(&self) {
        self.pool.close().await;
    }
}
