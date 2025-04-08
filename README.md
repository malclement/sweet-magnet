# 🔗 SweetMagnet

A BitTorrent client for handling magnet links written in Rust.

## Overview

SweetMagnet is a command-line utility for downloading content from BitTorrent magnet links, currently in development. The project implements core BitTorrent protocol functionality with a focus on security and performance.

## Current Implementation

The following components are currently implemented:

- **Magnet Link Parser**: Complete parsing of magnet URI format with support for extracting info hash, display name, and tracker lists
- **BitTorrent Protocol**:
    - Message encoding/decoding for all standard message types (handshake, choke, unchoke, interested, etc.)
    - Peer connection management with handshake implementation
    - Block and piece management with SHA-1 verification
- **Tracker Communication**:
    - HTTP tracker announce with compact peer response parsing
    - UDP tracker protocol implementation
    - Support for tracker events (started, stopped, completed)
- **Peer Discovery**:
    - Basic DHT (Distributed Hash Table) implementation
    - Bootstrap node connection
    - Peer exchange via trackers
- **Metadata Handling**:
    - `.torrent` file parsing
    - Support for single-file and multi-file torrents
    - Protocol extension for metadata exchange (BEP-9)
- **Configuration**:
    - JSON-based config file with defaults
    - Command-line parameter overrides

## Installation

```bash
git clone https://github.com/malclement/sweet-magnet.git
cd sweet-magnet
cargo build --release
```

## Usage

### Command Line Arguments

```bash
# Basic usage with a magnet link
cargo run -- --magnet "magnet:?xt=urn:btih:HASH&dn=NAME&tr=TRACKER"

# Specify a custom download directory
cargo run -- --magnet "magnet:?xt=urn:btih:HASH" --output-dir "./downloads"

# Enable verbose logging
cargo run -- --magnet "magnet:?xt=urn:btih:HASH" --verbose

# Specify a custom config file
cargo run -- --magnet "magnet:?xt=urn:btih:HASH" --config "my-config.json"
```

### Configuration File

The application supports a JSON configuration file with the following structure:

```json
{
  "download_dir": "./downloads",
  "max_concurrent_downloads": 3,
  "connection_timeout": 30,
  "use_dht": true
}
```

#### Configuration Options

| Option | Description | Default |
|--------|-------------|---------|
| `download_dir` | Directory to store downloaded files | `./downloads` |
| `max_concurrent_downloads` | Maximum number of simultaneous downloads | `3` |
| `connection_timeout` | Connection timeout in seconds | `30` |
| `use_dht` | Whether to use DHT for peer discovery | `true` |

## Project Structure

The codebase is organized into the following main components:

- **src/config.rs**: Configuration handling
- **src/error.rs**: Custom error types
- **src/protocol/**: BitTorrent protocol implementation
    - **message.rs**: BitTorrent message encoding/decoding
    - **peer.rs**: Peer connection management
    - **piece.rs**: Block and piece management
    - **tracker.rs**: HTTP tracker implementation
    - **udp_tacker.rs**: UDP tracker implementation
    - **dht.rs**: DHT implementation
    - **metadata.rs**: Torrent metadata handling
- **src/torrent/**: Torrent handling
    - **client.rs**: Main client implementation
    - **magnet.rs**: Magnet URI parsing
- **src/ui/**: User interface
    - **cli.rs**: Command-line interface

## Current Limitations

- The full end-to-end downloading functionality has some placeholder implementations
- The client creates placeholders instead of functional files for some operations
- Some peer wire protocol features like fast extension are not yet implemented
- Rate limiting is not yet implemented

## License

SweetMagnet is licensed under the MIT License. See the LICENSE file for details.
