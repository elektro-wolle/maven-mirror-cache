use axum_server::tls_rustls::RustlsConfig;
use config::{Config, Environment, File};
use serde_derive::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

// Repository configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Repository {
    pub name: String,
    pub url: String,
    pub priority: u8,
    #[serde(with = "humantime_serde")]
    pub timeout: Option<Duration>,
    pub headers: Option<HashMap<String, String>>,
}

// Main configuration structure
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProgramConfig {
    pub cache_dir: PathBuf,
    pub database_url: String,
    #[serde(with = "humantime_serde")]
    pub cache_ttl: Duration,
    #[serde(with = "humantime_serde")]
    pub negative_ttl: Duration,
    pub max_cache_size: u64,
    pub number_of_files_to_delete: u32,
    pub max_concurrent_downloads: usize,
    #[serde(with = "humantime_serde")]
    pub cleanup_interval: Duration,
    pub repositories: Vec<Repository>,
    pub server: ServerConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
    pub tls: Option<TlsConfig>,
}

impl ServerConfig {
    pub async fn create_tls_config(
        &self,
    ) -> Result<Option<RustlsConfig>, Box<dyn std::error::Error>> {
        let tls_config = match &self.tls {
            Some(config) => config,
            None => return Ok(None),
        };

        let config =
            RustlsConfig::from_pem_file(tls_config.cert_path.clone(), tls_config.key_path.clone())
                .await?;

        Ok(Some(config))
    }
}

impl Default for ProgramConfig {
    fn default() -> Self {
        Self {
            cache_dir: PathBuf::from("./cache"),
            database_url: "postgresql://postgres:password@localhost/maven_cache".to_string(),
            cache_ttl: Duration::new(30 * 86400, 0), // ca. 30 days
            negative_ttl: Duration::new(5 * 60, 0),  // 5 minutes
            max_cache_size: 1_000_000_000,
            number_of_files_to_delete: 1,
            max_concurrent_downloads: 10,
            cleanup_interval: Duration::new(3600, 0), //1 hour
            repositories: vec![
                Repository {
                    name: "maven-central".to_string(),
                    url: "https://repo1.maven.org/maven2".to_string(),
                    priority: 1,
                    timeout: Some(Duration::new(30, 0)),
                    headers: None,
                },
                Repository {
                    name: "maven-central-europe".to_string(),
                    url: "https://repo1.maven.org/maven2".to_string(),
                    priority: 2,
                    timeout: Some(Duration::new(30, 0)),
                    headers: None,
                },
            ],
            server: ServerConfig {
                host: "0.0.0.0".to_string(),
                port: 8080,
                request_timeout: Duration::new(30, 0),
                tls: None,
            },
        }
    }
}

// Load configuration from the file
pub fn load_config() -> Result<ProgramConfig, Box<dyn std::error::Error>> {
    // Initialize configuration
    let builder = Config::builder()
        // Load from `config.toml`
        .add_source(File::with_name("config").required(true))
        // Add environment variable overrides
        // Converts env vars like REPOSITORY__NAME to `repository.name`
        .add_source(Environment::default());

    // Build the configuration
    let config = builder.build()?;

    // Deserialize into the struct
    config.try_deserialize().map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_serialization() {
        let config = ProgramConfig::default();
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: ProgramConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.cache_ttl, parsed.cache_ttl);
    }
}
