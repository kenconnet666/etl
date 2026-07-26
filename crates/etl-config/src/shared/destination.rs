use secrecy::SecretString;
use serde::{Deserialize, Serialize};
#[cfg(feature = "utoipa")]
use utoipa::ToSchema;

/// Which source schema changes a destination follows.
///
/// Adding a column and renaming one only ever add information, so they are
/// always followed. Dropping a column, retyping one, and emptying a table on
/// `TRUNCATE` discard data in the replica, and a deployment may prefer to keep
/// it and reconcile by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(ToSchema))]
#[serde(deny_unknown_fields)]
pub struct SchemaFollowConfig {
    /// Whether a column dropped in the source is dropped in the destination.
    #[serde(default = "default_follow")]
    pub drop_column: bool,
    /// Whether a source column type change is applied to the destination.
    ///
    /// A destination that cannot promote the type in place rewrites the column,
    /// so a value the new type cannot represent fails the change.
    #[serde(default = "default_follow")]
    pub change_type: bool,
    /// Whether a source `TRUNCATE` empties the destination table.
    #[serde(default = "default_follow")]
    pub truncate: bool,
}

const fn default_follow() -> bool {
    true
}

impl Default for SchemaFollowConfig {
    fn default() -> Self {
        Self { drop_column: true, change_type: true, truncate: true }
    }
}

const fn default_ducklake_pool_size() -> u32 {
    DestinationConfig::DEFAULT_DUCKLAKE_POOL_SIZE
}

/// Runtime backend used for DuckLake external maintenance coordination.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum DuckLakeMaintenanceMode {
    #[default]
    Disabled,
    Postgres,
}

/// Supported product destination kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DestinationKind {
    /// Doris destination.
    Doris,
    /// DuckLake destination.
    Ducklake,
}

impl DestinationKind {
    /// Returns the stable destination name used in metrics and tags.
    pub const fn as_str(self) -> &'static str {
        match self {
            DestinationKind::Doris => "doris",
            DestinationKind::Ducklake => "ducklake",
        }
    }
}

/// Configuration for supported ETL data destinations.
///
/// Specifies the destination type and its associated configuration parameters.
/// Each variant corresponds to a different supported destination system.
///
/// This intentionally does not implement [`Serialize`] to avoid accidentally
/// leaking secrets in the config into serialized forms.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationConfig {
    Doris {
        /// Doris FE HTTP URL for Stream Load.
        fe_http_url: String,
        /// Doris FE MySQL protocol host.
        fe_mysql_host: String,
        /// Doris FE MySQL protocol port.
        fe_mysql_port: u16,
        /// Doris user name.
        user: String,
        /// Doris password.
        password: SecretString,
        /// Target Doris database.
        database: String,
        /// Optional Stream Load timeout in seconds.
        stream_load_timeout_secs: Option<u64>,
        /// Optional replica count for created tables. Doris defaults to three,
        /// which a cluster with fewer backends rejects.
        replication_num: Option<u16>,
        /// Which source schema changes to follow.
        #[serde(default)]
        schema_follow: SchemaFollowConfig,
    },
    Ducklake {
        /// DuckLake catalog URL.
        catalog_url: SecretString,
        /// DuckLake data path.
        data_path: String,
        /// Size of the DuckDB connection pool.
        #[serde(default = "default_ducklake_pool_size")]
        pool_size: u32,
        /// Optional S3-compatible storage access key ID.
        s3_access_key_id: Option<SecretString>,
        /// Optional S3-compatible storage secret access key.
        s3_secret_access_key: Option<SecretString>,
        /// Optional S3-compatible storage region.
        s3_region: Option<String>,
        /// Optional S3-compatible storage endpoint.
        s3_endpoint: Option<String>,
        /// Optional S3 URL style.
        s3_url_style: Option<String>,
        /// Optional S3 SSL toggle.
        s3_use_ssl: Option<bool>,
        /// Optional metadata schema for DuckLake metadata tables.
        metadata_schema: Option<String>,
        /// Optional DuckLake maintenance target file size.
        maintenance_target_file_size: Option<String>,
        /// Optional DuckLake snapshot-retention interval.
        expire_snapshots_older_than: Option<String>,
        /// External maintenance coordination backend.
        #[serde(default)]
        maintenance_mode: DuckLakeMaintenanceMode,
        /// Which source schema changes to follow.
        #[serde(default)]
        schema_follow: SchemaFollowConfig,
    },
}

impl DestinationConfig {
    /// Default connection pool size for DuckLake destinations.
    pub const DEFAULT_DUCKLAKE_POOL_SIZE: u32 = 4;

    /// Returns the destination kind represented by this config.
    pub fn kind(&self) -> DestinationKind {
        match self {
            DestinationConfig::Doris { .. } => DestinationKind::Doris,
            DestinationConfig::Ducklake { .. } => DestinationKind::Ducklake,
        }
    }
}

/// Same as [`DestinationConfig`] but without secrets. This type
/// implements [`Serialize`] because it does not contains secrets
/// so is safe to serialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationConfigWithoutSecrets {
    Doris {
        /// Doris FE HTTP URL for Stream Load.
        fe_http_url: String,
        /// Doris FE MySQL protocol host.
        fe_mysql_host: String,
        /// Doris FE MySQL protocol port.
        fe_mysql_port: u16,
        /// Doris user name.
        user: String,
        /// Target Doris database.
        database: String,
        /// Optional Stream Load timeout in seconds.
        stream_load_timeout_secs: Option<u64>,
        /// Optional replica count for created tables. Doris defaults to three,
        /// which a cluster with fewer backends rejects.
        replication_num: Option<u16>,
        /// Which source schema changes to follow.
        #[serde(default)]
        schema_follow: SchemaFollowConfig,
    },
    Ducklake {
        /// DuckLake data path.
        data_path: String,
        /// Size of the DuckDB connection pool.
        #[serde(default = "default_ducklake_pool_size")]
        pool_size: u32,
        /// Optional S3-compatible storage region.
        s3_region: Option<String>,
        /// Optional S3-compatible storage endpoint.
        s3_endpoint: Option<String>,
        /// Optional S3 URL style.
        s3_url_style: Option<String>,
        /// Optional S3 SSL toggle.
        s3_use_ssl: Option<bool>,
        /// Optional metadata schema for DuckLake metadata tables.
        metadata_schema: Option<String>,
        /// Optional DuckLake maintenance target file size.
        maintenance_target_file_size: Option<String>,
        /// Optional DuckLake snapshot-retention interval.
        expire_snapshots_older_than: Option<String>,
        /// External maintenance coordination backend.
        #[serde(default)]
        maintenance_mode: DuckLakeMaintenanceMode,
        /// Which source schema changes to follow.
        #[serde(default)]
        schema_follow: SchemaFollowConfig,
    },
}

impl From<DestinationConfig> for DestinationConfigWithoutSecrets {
    fn from(value: DestinationConfig) -> Self {
        match value {
            DestinationConfig::Doris {
                fe_http_url,
                fe_mysql_host,
                fe_mysql_port,
                user,
                password: _,
                database,
                stream_load_timeout_secs,
                replication_num,
                schema_follow,
            } => DestinationConfigWithoutSecrets::Doris {
                fe_http_url,
                fe_mysql_host,
                fe_mysql_port,
                user,
                database,
                stream_load_timeout_secs,
                replication_num,
                schema_follow,
            },
            DestinationConfig::Ducklake {
                catalog_url: _,
                data_path,
                pool_size,
                s3_access_key_id: _,
                s3_secret_access_key: _,
                s3_region,
                s3_endpoint,
                s3_url_style,
                s3_use_ssl,
                metadata_schema,
                maintenance_target_file_size,
                expire_snapshots_older_than,
                maintenance_mode,
                schema_follow,
            } => DestinationConfigWithoutSecrets::Ducklake {
                data_path,
                pool_size,
                s3_region,
                s3_endpoint,
                s3_url_style,
                s3_use_ssl,
                metadata_schema,
                maintenance_target_file_size,
                expire_snapshots_older_than,
                maintenance_mode,
                schema_follow,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ducklake_without_secrets_omits_catalog_url() {
        let config = DestinationConfig::Ducklake {
            catalog_url: "postgres://user:pass@localhost:5432/ducklake_catalog".to_owned().into(),
            data_path: "s3://bucket/path".to_owned(),
            pool_size: 4,
            s3_access_key_id: None,
            s3_secret_access_key: None,
            s3_region: None,
            s3_endpoint: None,
            s3_url_style: None,
            s3_use_ssl: None,
            metadata_schema: None,
            maintenance_target_file_size: None,
            expire_snapshots_older_than: None,
            maintenance_mode: DuckLakeMaintenanceMode::Postgres,
            schema_follow: SchemaFollowConfig::default(),
        };

        let without_secrets = DestinationConfigWithoutSecrets::from(config);
        let json = serde_json::to_value(without_secrets).unwrap();
        let serialized = json.to_string();

        assert!(!serialized.contains("catalog_url"));
        assert!(!serialized.contains("user:pass"));
    }

    #[test]
    fn doris_without_secrets_omits_password() {
        let config = DestinationConfig::Doris {
            fe_http_url: "http://localhost:8030".to_owned(),
            fe_mysql_host: "localhost".to_owned(),
            fe_mysql_port: 9030,
            user: "root".to_owned(),
            password: "secret123".to_owned().into(),
            database: "test_db".to_owned(),
            stream_load_timeout_secs: None,
            replication_num: None,
            schema_follow: SchemaFollowConfig::default(),
        };

        let without_secrets = DestinationConfigWithoutSecrets::from(config);
        let json = serde_json::to_value(without_secrets).unwrap();
        let serialized = json.to_string();

        assert!(!serialized.contains("password"));
        assert!(!serialized.contains("secret123"));
        assert!(serialized.contains("localhost"));
    }

    #[test]
    fn destination_kind_names_match_metrics_labels() {
        assert_eq!(DestinationKind::Doris.as_str(), "doris");
        assert_eq!(DestinationKind::Ducklake.as_str(), "ducklake");
    }
}
