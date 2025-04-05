# 🔗 SweetMagnet

A secure and fast magnet link streaming player written in Rust.

## Overview

SweetMagnet is a lightweight command-line utility for downloading content from torrent magnet links. It provides a simple, efficient way to handle BitTorrent downloads with minimal setup.

## Features

- Parse and validate magnet links
- Connect to BitTorrent trackers to discover peers
- Download content from magnet links
- Configurable download directory and options

## Installation

```bash
git clone https://github.com/malclement/sweet-magnet.git
cd sweet-magnet
cargo build --release
```

## Usage

### Running with Cargo

You can run the project directly with Cargo:

```bash
cargo run -- --magnet "magnet:?xt=urn:btih:HASH&dn=NAME&tr=TRACKER"
```

With custom output directory:

```bash
cargo run -- --magnet "magnet:?xt=urn:btih:HASH" --output-dir "./downloads"
```

Enable verbose logging:

```bash
cargo run -- --magnet "magnet:?xt=urn:btih:HASH" --verbose
```

### Running the compiled binary

After building, you can run the binary directly:

```bash
./target/release/sweet-magnet --magnet "magnet:?xt=urn:btih:HASH&dn=NAME&tr=TRACKER"
```

## Configuration

You can create a `config.json` file in the same directory as the executable with the following structure:

```json
{
  "download_dir": "./downloads",
  "max_concurrent_downloads": 3,
  "connection_timeout": 30,
  "use_dht": true
}
```

## License

SweetMagnet is licensed under the MIT License. See the LICENSE file for details.