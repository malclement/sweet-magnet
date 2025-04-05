use crate::config::Config;
use crate::error::SweetMagnetError;
use clap::Parser;
use log::{debug, info};
use std::path::Path;

/// Command line arguments
#[derive(Parser, Debug)]
#[clap(author, version, about)]
pub struct Args {
    /// Magnet link to download
    #[clap(short, long)]
    pub magnet: String,

    /// Output directory for downloaded content
    #[clap(short, long)]
    pub output_dir: Option<String>,

    /// Configuration file path
    #[clap(short, long, default_value = "./config.json")]
    pub config: String,

    /// Verbose output
    #[clap(short, long)]
    pub verbose: bool,
}

/// CLI handler for the application
pub struct Cli {
    pub args: Args,
    pub config: Config,
}

impl Cli {
    /// Initialize the CLI handler
    pub fn new() -> Result<Self, SweetMagnetError> {
        let args = Args::parse();

        // Load or create config
        let mut config = match Config::load(&args.config) {
            Ok(config) => {
                debug!("Loaded configuration from {}", args.config);
                config
            }
            Err(e) => {
                debug!("Failed to load config: {}, using defaults", e);
                Config::default()
            }
        };

        // Override config with command line args if provided
        if let Some(dir) = &args.output_dir {
            config.download_dir = dir.clone();
        }

        // Ensure the download directory exists
        let download_path = Path::new(&config.download_dir);
        if !download_path.exists() {
            std::fs::create_dir_all(download_path)
                .map_err(|e| SweetMagnetError::Io(e))?;
        }

        info!("Using download directory: {}", config.download_dir);

        Ok(Cli { args, config })
    }
}