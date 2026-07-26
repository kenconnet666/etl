//! Doris destination configuration.

use etl::{
    error::{ErrorKind, EtlResult},
    etl_error,
};
use url::Url;

/// Connection and load settings for the Doris destination.
#[derive(Clone, Debug)]
pub struct DorisConfig {
    /// Frontend HTTP base URL used for Stream Load.
    pub fe_http_url: Url,
    /// Frontend MySQL-protocol host.
    pub fe_mysql_host: String,
    /// Frontend MySQL-protocol port.
    pub fe_mysql_port: u16,
    /// Doris user with load and DDL privileges.
    pub user: String,
    /// Password for the Doris user.
    pub password: String,
    /// Target database.
    pub database: String,
    /// Timeout applied to one Stream Load request, in seconds.
    pub stream_load_timeout_secs: u64,
    /// Timeout while waiting for an async schema change, in seconds.
    pub schema_change_timeout_secs: u64,
    /// Pipeline ID used in Stream Load labels.
    pub pipeline_id: u64,
    /// Replica count for created tables, or [`None`] to accept the Doris
    /// default. A cluster with a single backend requires one.
    pub replication_num: Option<u16>,
}

impl DorisConfig {
    /// Default Stream Load request timeout.
    pub const DEFAULT_STREAM_LOAD_TIMEOUT_SECS: u64 = 600;
    /// Default asynchronous schema-change wait timeout.
    pub const DEFAULT_SCHEMA_CHANGE_TIMEOUT_SECS: u64 = 1800;

    /// Returns the Stream Load URL for one table.
    pub(super) fn stream_load_url(&self, table: &str) -> EtlResult<Url> {
        let path = format!("/api/{}/{}/_stream_load", self.database, table);
        self.fe_http_url.join(&path).map_err(|source| {
            etl_error!(
                ErrorKind::ConfigError,
                "Doris stream load URL is invalid",
                format!("base={} path={path}", self.fe_http_url),
                source: source
            )
        })
    }

    /// Returns the MySQL-protocol connection URL used for DDL.
    ///
    /// The target database is left out because the destination creates it on
    /// first use, and every statement names its database explicitly.
    pub(super) fn mysql_url(&self) -> String {
        format!(
            "mysql://{}:{}@{}:{}",
            percent_encode(&self.user),
            percent_encode(&self.password),
            self.fe_mysql_host,
            self.fe_mysql_port
        )
    }
}

/// Percent-encodes characters unsafe in a URL userinfo component.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(ch),
            _ => {
                let mut buf = [0u8; 4];
                for byte in ch.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> DorisConfig {
        DorisConfig {
            fe_http_url: Url::parse("http://127.0.0.1:8030").unwrap(),
            fe_mysql_host: "127.0.0.1".to_owned(),
            fe_mysql_port: 9030,
            user: "etl".to_owned(),
            password: "placeholder-password".to_owned(),
            database: "analytics".to_owned(),
            stream_load_timeout_secs: DorisConfig::DEFAULT_STREAM_LOAD_TIMEOUT_SECS,
            schema_change_timeout_secs: DorisConfig::DEFAULT_SCHEMA_CHANGE_TIMEOUT_SECS,
            pipeline_id: 1,
            replication_num: None,
        }
    }

    #[test]
    fn stream_load_url_targets_the_configured_database() {
        let url = test_config().stream_load_url("public_users").unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:8030/api/analytics/public_users/_stream_load");
    }

    #[test]
    fn mysql_url_encodes_userinfo_and_omits_the_database() {
        let mut config = test_config();
        config.password = "p@ss:word/1".to_owned();
        assert_eq!(config.mysql_url(), "mysql://etl:p%40ss%3Aword%2F1@127.0.0.1:9030");
    }
}
