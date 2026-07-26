use etl_config::{load_config, shared::ReplicatorConfig};

use crate::error::{ReplicatorError, ReplicatorResult};

/// Loads and validates the replicator configuration.
pub(crate) fn init() -> ReplicatorResult<ReplicatorConfig> {
    let config = load_config::<ReplicatorConfig>().map_err(ReplicatorError::config)?;
    config.validate().map_err(ReplicatorError::config)?;

    Ok(config)
}

#[cfg(test)]
mod tests {
    use etl_config::shared::DestinationConfig;

    use super::*;

    /// Loads a shipped environment file through the real loader.
    ///
    /// Nextest runs one process per test, so setting the loader's environment
    /// variables here cannot affect another test.
    fn load_environment(environment: &str) -> ReplicatorConfig {
        let configuration_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("configuration");
        // SAFETY: nextest gives each test its own process, so no other thread
        // can observe this environment while it is being set.
        unsafe {
            std::env::set_var("APP_CONFIG_DIR", &configuration_dir);
            std::env::set_var("APP_ENVIRONMENT", environment);
        }

        load_config::<ReplicatorConfig>().expect("shipped configuration should load")
    }

    #[test]
    fn dev_configuration_describes_the_local_stack() {
        let config = load_environment("dev");

        config.validate().expect("shipped configuration should validate");
        assert_eq!(config.pipeline.pg_connection.port, 15432);
        assert_eq!(config.pipeline.publication_name, "dbz_publication");

        let DestinationConfig::Ducklake { data_path, s3_endpoint, s3_url_style, .. } =
            &config.destination
        else {
            panic!("the dev environment should target DuckLake");
        };
        assert_eq!(data_path, "s3://lake/ducklake");
        assert_eq!(s3_endpoint.as_deref(), Some("localhost:19000"));
        assert_eq!(s3_url_style.as_deref(), Some("path"));
    }
}
