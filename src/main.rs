use anyhow::{Context, Result};
use env_logger::Env;
use log::{debug, error, info, warn};

mod config;
mod error;
mod torrent;
mod ui;

use error::SweetMagnetError;
use torrent::{MagnetLink, TorrentClient};
use ui::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize CLI and parse arguments
    let cli = Cli::new().context("Failed to initialize CLI")?;

    // Initialize logging
    initialize_logging(cli.args.verbose);

    info!("Starting SweetMagnet version {}", env!("CARGO_PKG_VERSION"));
    debug!("Using config: {:?}", cli.config);

    // Parse the magnet link
    let magnet_link = match MagnetLink::parse(&cli.args.magnet) {
        Ok(link) => link,
        Err(e) => {
            error!("Failed to parse magnet link: {}", e);
            return Err(anyhow::anyhow!(e));
        }
    };

    info!("Parsed magnet link: {}", magnet_link.name());
    debug!("Magnet info hash: {}", magnet_link.info_hash());
    debug!("Found {} trackers", magnet_link.trackers.len());

    // Create and initialize the torrent client
    let client = TorrentClient::new(magnet_link, &cli.config.download_dir);
    client.initialize().await.context("Failed to initialize torrent client")?;

    // Start downloading
    match client.download().await {
        Ok(file_path) => {
            info!("Successfully downloaded to: {}", file_path);
        }
        Err(e) => {
            error!("Download failed: {}", e);
            return Err(anyhow::anyhow!(e));
        }
    }

    info!("Download completed successfully!");
    Ok(())
}

fn initialize_logging(verbose: bool) {
    let env = if verbose {
        Env::default().default_filter_or("debug")
    } else {
        Env::default().default_filter_or("info")
    };

    env_logger::Builder::from_env(env)
        .format_timestamp_secs()
        .init();
}