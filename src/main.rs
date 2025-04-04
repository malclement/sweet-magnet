use anyhow::{Context, Result};
use clap::Parser;
use log::{info, warn, error};

/// Command line arguments
#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    /// Magnet link to stream
    #[clap(short, long)]
    magnet: String,

    /// Output directory for downloaded content
    #[clap(short, long, default_value = "./temp")]
    output_dir: String,

    /// Verbose output
    #[clap(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    // Parse command line arguments
    let args = Args::parse();

    // Initialize logger
    if args.verbose {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    }

    info!("Starting SweetMagnet...");
    info!("Magnet link: {}", args.magnet);

    // TODO: Initialize the torrent client

    // TODO: Start downloading and streaming the content

    Ok(())
}