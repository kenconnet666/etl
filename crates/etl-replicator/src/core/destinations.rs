//! Destination startup dispatch.

use etl_config::shared::{DestinationKind, ReplicatorConfig};

use super::ReplicatorStore;
use crate::error::ReplicatorResult;

/// Starts the configured destination pipeline.
pub(super) async fn start(
    replicator_config: ReplicatorConfig,
    store: ReplicatorStore,
) -> ReplicatorResult<()> {
    #[cfg(not(feature = "any-destination"))]
    let _ = &store;

    match replicator_config.destination.kind() {
        DestinationKind::Ducklake => {
            #[cfg(feature = "ducklake")]
            {
                ducklake::start(replicator_config, store).await
            }

            #[cfg(not(feature = "ducklake"))]
            {
                Err(disabled_destination_error(DestinationKind::Ducklake))
            }
        }
    }
}

#[cfg(not(feature = "ducklake"))]
fn disabled_destination_error(kind: DestinationKind) -> crate::error::ReplicatorError {
    crate::error::ReplicatorError::config(std::io::Error::other(format!(
        "Destination `{}` support is not compiled into this binary.",
        kind.as_str()
    )))
}

/// DuckLake destination startup.
#[cfg(feature = "ducklake")]
mod ducklake {
    use etl::pipeline::Pipeline;
    use etl_config::{
        default_ducklake_s3_url_style, default_ducklake_s3_use_ssl, parse_ducklake_s3_data_path,
        parse_ducklake_url,
        shared::{
            DestinationConfig, DuckLakeMaintenanceMode as ConfigDuckLakeMaintenanceMode,
            ReplicatorConfig,
        },
    };
    use etl_destinations::ducklake::{
        DuckLakeDestination, DuckLakeExternalMaintenanceConfig, DuckLakeMaintenanceMode,
        S3Config as DucklakeS3Config,
    };
    use secrecy::ExposeSecret;

    use super::super::{ReplicatorStore, pipeline};
    use crate::error::{ReplicatorError, ReplicatorResult};

    /// Starts the DuckLake destination pipeline.
    pub(super) async fn start(
        replicator_config: ReplicatorConfig,
        store: ReplicatorStore,
    ) -> ReplicatorResult<()> {
        let pipeline_id = replicator_config.pipeline.id;

        let DestinationConfig::Ducklake {
            catalog_url,
            data_path,
            pool_size,
            s3_access_key_id,
            s3_secret_access_key,
            s3_region,
            s3_endpoint,
            s3_url_style,
            s3_use_ssl,
            metadata_schema,
            maintenance_target_file_size,
            expire_snapshots_older_than,
            maintenance_mode,
        } = &replicator_config.destination;

        let s3_config = match (s3_access_key_id, s3_secret_access_key) {
            (Some(access_key_id), Some(secret_access_key)) => Some(DucklakeS3Config {
                access_key_id: access_key_id.expose_secret().to_owned(),
                secret_access_key: secret_access_key.expose_secret().to_owned(),
                region: s3_region.clone().unwrap_or_else(|| "us-east-1".to_owned()),
                endpoint: s3_endpoint.clone(),
                url_style: s3_url_style.clone().unwrap_or_else(|| {
                    default_ducklake_s3_url_style(s3_endpoint.as_deref()).to_owned()
                }),
                use_ssl: s3_use_ssl.unwrap_or_else(default_ducklake_s3_use_ssl),
            }),
            (None, None) => None,
            _ => {
                return Err(ReplicatorError::config(std::io::Error::other(
                    "DuckLake S3 credentials must include both access key id and secret access key",
                )));
            }
        };

        let maintenance_mode = match maintenance_mode {
            ConfigDuckLakeMaintenanceMode::Disabled => DuckLakeMaintenanceMode::Disabled,
            ConfigDuckLakeMaintenanceMode::Postgres => DuckLakeMaintenanceMode::Postgres,
        };
        let external_maintenance =
            DuckLakeExternalMaintenanceConfig { mode: maintenance_mode, pipeline_id };

        let destination = DuckLakeDestination::new_with_external_maintenance(
            parse_ducklake_url(catalog_url.expose_secret()).map_err(ReplicatorError::config)?,
            parse_ducklake_s3_data_path(data_path).map_err(ReplicatorError::config)?,
            *pool_size,
            s3_config,
            metadata_schema.clone(),
            maintenance_target_file_size.clone(),
            expire_snapshots_older_than.clone(),
            external_maintenance,
            store.clone(),
        )
        .await?;

        let pipeline = Pipeline::new(replicator_config.pipeline, store, destination);
        pipeline::start(pipeline).await
    }
}
