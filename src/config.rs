use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Application configuration
#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    /// Directory to store downloaded files
    pub download_dir: String,
    /// Maximum concurrent downloads
    pub max_concurrent_downloads: usize,
    /// Connection timeout in seconds
    pub connection_timeout: u64,
    /// Whether to use DHT for peer discovery
    pub use_dht: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            download_dir: "./downloads".to_string(),
            max_concurrent_downloads: 3,
            connection_timeout: 30,
            use_dht: true,
        }
    }
}

impl Config {
    /// Load configuration from a file, or create default if it doesn't exist
    pub fn load(path: &str) -> Result<Self> {
        let config_path = Path::new(path);

        if config_path.exists() {
            let config_str = fs::read_to_string(config_path)?;
            let config = serde_json::from_str(&config_str)?;
            Ok(config)
        } else {
            let config = Config::default();
            Ok(config)
        }
    }

    /// Save configuration to a file
    pub fn save(&self, path: &str) -> Result<()> {
        let config_str = serde_json::to_string_pretty(self)?;
        fs::write(path, config_str)?;
        Ok(())
    }
}