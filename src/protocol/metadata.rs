//! Torrent metadata handling

use anyhow::{Context, Result};
use log::{debug, info, warn, error};
use serde::{Deserialize, Serialize};
use serde_bencode::value::Value;
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::net::SocketAddr;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{timeout, Duration};
use tokio::net::TcpStream;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Instant;
use rand::Rng;
use url::Url;
use crate::protocol::{Dht, Tracker, TrackerEvent, UdpTracker};

/// A file in the torrent
#[derive(Debug, Clone)]
pub struct TorrentFile {
    pub path: PathBuf,
    pub length: u64,
    pub offset: u64, // Offset in the complete torrent data
}

/// Torrent metadata
#[derive(Debug, Clone)]
pub struct TorrentMetadata {
    pub info_hash: [u8; 20],
    pub name: String,
    pub piece_length: usize,
    pub pieces: Vec<[u8; 20]>,
    pub total_length: u64,
    pub files: Vec<TorrentFile>,
    pub is_single_file: bool,
}

/// Raw info dictionary from a .torrent file
#[derive(Debug, Deserialize)]
struct InfoDict {
    name: String,
    #[serde(rename = "piece length")]
    piece_length: usize,
    pieces: Vec<u8>,
    #[serde(default)]
    length: Option<u64>,
    #[serde(default)]
    files: Option<Vec<FileDict>>,
}

/// Raw file entry from a .torrent file
#[derive(Debug, Deserialize)]
struct FileDict {
    length: u64,
    path: Vec<String>,
}

/// Events for peer communication
#[derive(Debug)]
enum PeerEvent {
    Connected(SocketAddr),
    Disconnected(SocketAddr, Option<String>),
    MetadataReceived(Bytes),
    MetadataPieceReceived(usize, Bytes),
    MetadataSize(usize),
    Error(SocketAddr, String),
}

impl TorrentMetadata {
    /// Create metadata from a .torrent file
    pub fn from_torrent_file(torrent_path: &Path) -> Result<Self> {
        let torrent_bytes = std::fs::read(torrent_path)
            .context("Failed to read torrent file")?;

        let torrent: BTreeMap<String, Value> = serde_bencode::from_bytes(&torrent_bytes)
            .context("Failed to parse torrent file")?;

        // Extract the 'info' dictionary
        let info = torrent.get("info")
            .ok_or_else(|| anyhow::anyhow!("No 'info' dictionary in torrent file"))?;

        // Calculate info hash
        let info_bytes = serde_bencode::to_bytes(info)
            .context("Failed to encode info dictionary")?;

        let mut hasher = Sha1::new();
        hasher.update(&info_bytes);
        let result = hasher.finalize();

        let mut info_hash = [0u8; 20];
        info_hash.copy_from_slice(&result);

        // Parse the info dictionary by re-encoding and then decoding
        let info_dict: InfoDict = serde_bencode::from_bytes(&info_bytes)
            .context("Failed to parse info dictionary")?;

        // Get piece hashes
        let pieces_bytes = &info_dict.pieces;
        let mut pieces = Vec::new();

        for chunk in pieces_bytes.chunks(20) {
            if chunk.len() == 20 {
                let mut hash = [0u8; 20];
                hash.copy_from_slice(chunk);
                pieces.push(hash);
            } else {
                return Err(anyhow::anyhow!("Invalid piece hash length"));
            }
        }

        info!("Found {} pieces in torrent", pieces.len());

        // Determine if single or multi-file
        let (total_length, files, is_single_file) = if let Some(length) = info_dict.length {
            // Single file torrent
            let file = TorrentFile {
                path: PathBuf::from(&info_dict.name),
                length,
                offset: 0,
            };
            (length, vec![file], true)
        } else if let Some(files_info) = info_dict.files {
            // Multi-file torrent
            let mut files = Vec::new();
            let mut offset = 0;
            let mut total_length = 0;

            for file_info in files_info {
                let mut path = PathBuf::from(&info_dict.name);
                for part in &file_info.path {
                    path.push(part);
                }

                let file = TorrentFile {
                    path,
                    length: file_info.length,
                    offset,
                };

                offset += file_info.length;
                total_length += file_info.length;
                files.push(file);
            }

            (total_length, files, false)
        } else {
            return Err(anyhow::anyhow!("Invalid torrent structure - missing files information"));
        };

        debug!("Torrent has {} file(s) with total size {} bytes",
               files.len(), total_length);

        Ok(TorrentMetadata {
            info_hash,
            name: info_dict.name,
            piece_length: info_dict.piece_length,
            pieces,
            total_length,
            files,
            is_single_file,
        })
    }

    /// Calculate piece lengths
    pub fn get_piece_lengths(&self) -> Vec<usize> {
        let num_pieces = self.pieces.len();

        if num_pieces == 0 {
            return Vec::new();
        }

        let mut piece_lengths = Vec::with_capacity(num_pieces);

        // All pieces except the last one are of standard length
        for _ in 0..(num_pieces - 1) {
            piece_lengths.push(self.piece_length);
        }

        // Last piece may be shorter
        let last_piece_length = self.total_length as usize % self.piece_length;
        if last_piece_length == 0 {
            piece_lengths.push(self.piece_length);
        } else {
            piece_lengths.push(last_piece_length);
        }

        piece_lengths
    }

    /// Get the file containing a particular piece
    pub fn get_files_for_piece(&self, piece_index: usize) -> Vec<(usize, u64, u64)> {
        let mut result = Vec::new();
        let piece_start = piece_index * self.piece_length;
        let piece_end = std::cmp::min(
            piece_start + self.piece_length,
            self.total_length as usize,
        );

        for (file_index, file) in self.files.iter().enumerate() {
            let file_start = file.offset as usize;
            let file_end = (file.offset + file.length) as usize;

            // Check if this file overlaps with the piece
            if file_end > piece_start && file_start < piece_end {
                // Calculate overlap
                let overlap_start = std::cmp::max(file_start, piece_start);
                let overlap_end = std::cmp::min(file_end, piece_end);

                // Calculate offsets within piece and file
                let piece_offset = overlap_start - piece_start;
                let file_offset = overlap_start - file_start;
                let length = overlap_end - overlap_start;

                result.push((file_index, file_offset as u64, length as u64));
            }
        }

        result
    }

    /// Create metadata from a magnet link (fetches metadata from peers)
    pub async fn from_magnet(info_hash: [u8; 20], name: &str, trackers: &[String]) -> Result<Self> {
        // Constants for metadata exchange
        const METADATA_PIECE_SIZE: usize = 16384; // 16 KiB
        const MAX_METADATA_SIZE: usize = 10 * 1024 * 1024; // 10 MiB
        const METADATA_FETCH_TIMEOUT: u64 = 60; // 60 seconds
        const MAX_PEERS: usize = 50;
        const MAX_CONCURRENT_CONNECTIONS: usize = 10;

        info!("Fetching metadata for magnet link with info hash: {:?}", info_hash);

        // Generate a random peer ID (20 bytes)
        let mut peer_id = [0u8; 20];
        let mut rng = rand::thread_rng();
        rng.fill(&mut peer_id[..]);

        // Create a channel for collecting peers from different sources
        let (peer_tx, mut peer_rx) = mpsc::channel(100);
        let all_peers = Arc::new(Mutex::new(HashSet::new()));

        // Start tasks to fetch peers from trackers and DHT in parallel

        // 1. Fetch peers from HTTP trackers
        let http_trackers: Vec<_> = trackers.iter()
            .filter(|t| t.starts_with("http"))
            .cloned()
            .collect();

        if !http_trackers.is_empty() {
            let info_hash_clone = info_hash;
            let peer_id_clone = peer_id;
            let peer_tx_clone = peer_tx.clone();
            let all_peers_clone = all_peers.clone();

            tokio::spawn(async move {
                info!("Fetching peers from {} HTTP trackers", http_trackers.len());

                for tracker_url in http_trackers {
                    match Url::parse(&tracker_url) {
                        Ok(_) => {
                            let tracker = Tracker::new(
                                &tracker_url,
                                info_hash_clone,
                                peer_id_clone,
                                0, // We don't know the size yet
                            );

                            match tracker.announce(TrackerEvent::Started).await {
                                Ok(response) => {
                                    info!("Got {} peers from HTTP tracker: {}", response.peers.len(), tracker_url);

                                    // Add peers to the set and send to channel
                                    let new_peers = {
                                        let mut all_peers = all_peers_clone.lock().await;
                                        let mut new_count = 0;

                                        for peer_addr in response.peers {
                                            if all_peers.insert(peer_addr) {
                                                new_count += 1;
                                                if let Err(e) = peer_tx_clone.send(peer_addr).await {
                                                    error!("Failed to send peer to channel: {}", e);
                                                    break;
                                                }
                                            }
                                        }

                                        new_count
                                    };

                                    debug!("Added {} new unique peers from HTTP tracker", new_peers);
                                },
                                Err(e) => {
                                    warn!("Failed to get peers from HTTP tracker {}: {}", tracker_url, e);
                                }
                            }
                        },
                        Err(e) => {
                            warn!("Invalid tracker URL {}: {}", tracker_url, e);
                        }
                    }
                }
            });
        }

        // 2. Fetch peers from UDP trackers
        let udp_trackers: Vec<_> = trackers.iter()
            .filter(|t| t.starts_with("udp"))
            .cloned()
            .collect();

        if !udp_trackers.is_empty() {
            let info_hash_clone = info_hash;
            let peer_id_clone = peer_id;
            let peer_tx_clone = peer_tx.clone();
            let all_peers_clone = all_peers.clone();

            tokio::spawn(async move {
                info!("Fetching peers from {} UDP trackers", udp_trackers.len());

                for tracker_url in udp_trackers {
                    match UdpTracker::new(&tracker_url).await {
                        Ok(mut tracker) => {
                            match tracker.announce(
                                info_hash_clone,
                                peer_id_clone,
                                0, // downloaded
                                0, // left (unknown)
                                0, // uploaded
                                2, // event: started
                                6881, // port
                            ).await {
                                Ok(peers) => {
                                    info!("Got {} peers from UDP tracker: {}", peers.len(), tracker_url);

                                    // Add peers to the set and send to channel
                                    let new_peers = {
                                        let mut all_peers = all_peers_clone.lock().await;
                                        let mut new_count = 0;

                                        for peer_addr in peers {
                                            if all_peers.insert(peer_addr) {
                                                new_count += 1;
                                                if let Err(e) = peer_tx_clone.send(peer_addr).await {
                                                    error!("Failed to send peer to channel: {}", e);
                                                    break;
                                                }
                                            }
                                        }

                                        new_count
                                    };

                                    debug!("Added {} new unique peers from UDP tracker", new_peers);
                                },
                                Err(e) => {
                                    warn!("Failed to get peers from UDP tracker {}: {}", tracker_url, e);
                                }
                            }
                        },
                        Err(e) => {
                            warn!("Failed to initialize UDP tracker {}: {}", tracker_url, e);
                        }
                    }
                }
            });
        }

        // 3. Fetch peers from DHT
        let dht_enabled = true; // Could be a config option

        if dht_enabled {
            let info_hash_clone = info_hash;
            let peer_tx_clone = peer_tx.clone();
            let all_peers_clone = all_peers.clone();

            tokio::spawn(async move {
                info!("Fetching peers from DHT");

                match Dht::new().await {
                    Ok(mut dht) => {
                        match dht.find_peers(info_hash_clone, 30).await {
                            Ok(peers) => {
                                info!("Got {} peers from DHT", peers.len());

                                // Add peers to the set and send to channel
                                let new_peers = {
                                    let mut all_peers = all_peers_clone.lock().await;
                                    let mut new_count = 0;

                                    for peer_addr in peers {
                                        if all_peers.insert(peer_addr) {
                                            new_count += 1;
                                            if let Err(e) = peer_tx_clone.send(peer_addr).await {
                                                error!("Failed to send peer to channel: {}", e);
                                                break;
                                            }
                                        }
                                    }

                                    new_count
                                };

                                debug!("Added {} new unique peers from DHT", new_peers);
                            },
                            Err(e) => {
                                warn!("Failed to get peers from DHT: {}", e);
                            }
                        }
                    },
                    Err(e) => {
                        warn!("Failed to initialize DHT: {}", e);
                    }
                }
            });
        }

        // Drop the original sender so the channel can close when all tasks are done
        drop(peer_tx);

        // Wait for some initial peers (with timeout)
        let start_time = Instant::now();
        let peer_discovery_timeout = Duration::from_secs(10);
        let mut peers = Vec::new();

        // Collect peers until we have enough or timeout
        while let Ok(Some(peer)) = timeout(
            peer_discovery_timeout.saturating_sub(start_time.elapsed()),
            peer_rx.recv()
        ).await {
            peers.push(peer);

            // If we have enough peers, break
            if peers.len() >= MAX_PEERS {
                break;
            }

            // Check timeout
            if start_time.elapsed() >= peer_discovery_timeout {
                break;
            }
        }

        // Continue collecting peers in the background
        let (extra_peers_tx, mut extra_peers_rx) = mpsc::channel(100);
        tokio::spawn(async move {
            while let Some(peer) = peer_rx.recv().await {
                if let Err(_) = extra_peers_tx.send(peer).await {
                    break;
                }
            }
        });

        if peers.is_empty() {
            return Err(anyhow::anyhow!("No peers available to fetch metadata"));
        }

        info!("Found {} peers for metadata exchange", peers.len());

        // Create a channel for receiving metadata
        let (metadata_tx, mut metadata_rx) = mpsc::channel(1);
        let mut active_tasks = 0;
        let max_tasks = MAX_CONCURRENT_CONNECTIONS;

        // Try to fetch metadata from peers concurrently (with limits)
        let metadata_result = timeout(
            Duration::from_secs(METADATA_FETCH_TIMEOUT),
            async {
                let mut peer_index = 0;

                // Process peers until we get metadata or run out of peers
                loop {
                    // Start new tasks if we have peers and are under the limit
                    while active_tasks < max_tasks && peer_index < peers.len() {
                        let peer_addr = peers[peer_index];
                        peer_index += 1;

                        let info_hash_clone = info_hash;
                        let peer_id_clone = peer_id;
                        let metadata_tx_clone = metadata_tx.clone();

                        active_tasks += 1;
                        tokio::spawn(async move {
                            match fetch_from_peer(peer_addr, info_hash_clone, peer_id_clone).await {
                                Ok(metadata) => {
                                    if let Err(_) = metadata_tx_clone.send(metadata).await {
                                        // Channel is closed, receiver got metadata already
                                    }
                                },
                                Err(e) => {
                                    debug!("Failed to fetch metadata from {}: {}", peer_addr, e);
                                }
                            }
                        });
                    }

                    // Check if we received metadata
                    if let Ok(metadata) = metadata_rx.try_recv() {
                        return Ok(metadata);
                    }

                    // Try to get more peers if available
                    if peer_index >= peers.len() {
                        if let Ok(peer) = extra_peers_rx.try_recv() {
                            peers.push(peer);
                        } else if active_tasks == 0 {
                            // No more peers and no active tasks
                            return Err(anyhow::anyhow!("Failed to fetch metadata from any peer"));
                        }
                    }

                    // Wait a bit to avoid spinning
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        ).await;

        // Process the result
        match metadata_result {
            Ok(Ok(metadata_bytes)) => {
                // Parse the metadata
                parse_metadata(&metadata_bytes, info_hash, name)
            },
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow::anyhow!("Metadata fetch timed out"))
        }
    }
}

/// Parse raw metadata bytes into a TorrentMetadata structure
fn parse_metadata(metadata_bytes: &BytesMut, info_hash: [u8; 20], name: &str) -> Result<TorrentMetadata> {
    // Parse the bencode format
    let info_dict: InfoDict = serde_bencode::from_bytes(metadata_bytes)
        .context("Failed to parse metadata")?;

    // Extract piece hashes
    let pieces_bytes = &info_dict.pieces;
    let mut pieces = Vec::new();

    for chunk in pieces_bytes.chunks(20) {
        if chunk.len() == 20 {
            let mut hash = [0u8; 20];
            hash.copy_from_slice(chunk);
            pieces.push(hash);
        } else {
            return Err(anyhow::anyhow!("Invalid piece hash length"));
        }
    }

    info!("Found {} pieces in magnet metadata", pieces.len());

    // Determine if single or multi-file
    let (total_length, files, is_single_file) = if let Some(length) = info_dict.length {
        // Single file torrent
        let file = TorrentFile {
            path: PathBuf::from(&info_dict.name),
            length,
            offset: 0,
        };
        (length, vec![file], true)
    } else if let Some(files_info) = info_dict.files {
        // Multi-file torrent
        let mut files = Vec::new();
        let mut offset = 0;
        let mut total_length = 0;

        for file_info in files_info {
            let mut path = PathBuf::from(&info_dict.name);
            for part in &file_info.path {
                path.push(part);
            }

            let file = TorrentFile {
                path,
                length: file_info.length,
                offset,
            };

            offset += file_info.length;
            total_length += file_info.length;
            files.push(file);
        }

        (total_length, files, false)
    } else {
        return Err(anyhow::anyhow!("Invalid torrent structure - missing files information"));
    };

    debug!("Magnet metadata has {} file(s) with total size {} bytes",
           files.len(), total_length);

    Ok(TorrentMetadata {
        info_hash,
        name: info_dict.name,
        piece_length: info_dict.piece_length,
        pieces,
        total_length,
        files,
        is_single_file,
    })
}

/// Fetch metadata from a peer using extension protocol
async fn fetch_from_peer(
    peer_addr: SocketAddr,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
) -> Result<BytesMut> {
    // Constants for the protocol
    const BT_PROTOCOL: &[u8] = b"BitTorrent protocol";
    const EXTENSION_BIT: u8 = 0x10; // Extended messaging protocol bit (5th byte)
    const HANDSHAKE_SIZE: usize = 68; // 1 + 19 + 8 + 20 + 20
    const EXT_HANDSHAKE_ID: u8 = 0;

    // ut_metadata message types
    const REQUEST: u8 = 0;
    const DATA: u8 = 1;
    const REJECT: u8 = 2;

    const METADATA_PIECE_SIZE: usize = 16384; // 16 KiB
    const MAX_METADATA_SIZE: usize = 10 * 1024 * 1024; // 10 MiB

    debug!("Connecting to peer at {}", peer_addr);

    // Connect to the peer with timeout
    let mut stream = match timeout(
        Duration::from_secs(5),
        TcpStream::connect(peer_addr),
    ).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => return Err(anyhow::anyhow!("Failed to connect to peer: {}", e)),
        Err(_) => return Err(anyhow::anyhow!("Connection to peer timed out")),
    };

    debug!("Connected to peer at {}", peer_addr);

    // Prepare the handshake
    let mut handshake = BytesMut::with_capacity(HANDSHAKE_SIZE);

    // Protocol name length
    handshake.put_u8(BT_PROTOCOL.len() as u8);
    // Protocol name
    handshake.put_slice(BT_PROTOCOL);

    // Reserved bytes - set the extension protocol bit
    let mut reserved = [0u8; 8];
    reserved[5] |= EXTENSION_BIT; // Enable extended messaging protocol
    handshake.put_slice(&reserved);

    // Info hash and peer ID
    handshake.put_slice(&info_hash);
    handshake.put_slice(&peer_id);

    // Send handshake
    if let Err(e) = stream.write_all(&handshake).await {
        return Err(anyhow::anyhow!("Failed to send handshake: {}", e));
    }

    // Receive handshake response
    let mut response = BytesMut::with_capacity(HANDSHAKE_SIZE);
    response.resize(HANDSHAKE_SIZE, 0);

    if let Err(e) = timeout(Duration::from_secs(5), stream.read_exact(&mut response)).await {
        return Err(anyhow::anyhow!("Handshake response timed out: {}", e));
    }

    // Check if extended messaging protocol is supported
    if response[25] & EXTENSION_BIT == 0 {
        return Err(anyhow::anyhow!("Peer does not support extended messaging protocol"));
    }

    // Check info hash in response
    let response_info_hash = &response[28..48];
    if response_info_hash != info_hash {
        return Err(anyhow::anyhow!("Info hash mismatch in handshake response"));
    }

    debug!("Handshake with peer at {} successful", peer_addr);

    // Send extended handshake
    let mut ext_handshake = HashMap::new();
    let mut m = HashMap::new();
    // Use byte vectors for keys instead of strings
    m.insert(b"ut_metadata".to_vec(), Value::Int(1)); // Ask for ut_metadata extension
    ext_handshake.insert(b"m".to_vec(), Value::Dict(m));
    ext_handshake.insert(b"metadata_size".to_vec(), Value::Int(0)); // We don't know the size yet

    let ext_handshake_encoded = serde_bencode::to_bytes(&ext_handshake)
        .context("Failed to encode extended handshake")?;

    // Send extended handshake message
    // Format: <length prefix><message ID = 20><extended message ID = 0><payload>
    let mut message = BytesMut::new();
    message.put_u32(1 + 1 + ext_handshake_encoded.len() as u32); // length prefix
    message.put_u8(20); // extended message ID
    message.put_u8(EXT_HANDSHAKE_ID); // extended handshake ID
    message.put_slice(&ext_handshake_encoded);

    if let Err(e) = stream.write_all(&message).await {
        return Err(anyhow::anyhow!("Failed to send extended handshake: {}", e));
    }

    debug!("Sent extended handshake to peer at {}", peer_addr);

    // Process messages and fetch metadata
    let mut metadata_size = 0;
    let mut ut_metadata_id = 0;
    let mut metadata_pieces = HashMap::new();
    let mut metadata = None;

    // Set a timeout for the entire metadata fetch operation
    let start_time = Instant::now();
    let timeout_duration = Duration::from_secs(30);

    while start_time.elapsed() < timeout_duration {
        // Read message length
        let mut len_buf = [0u8; 4];
        match timeout(Duration::from_secs(5), stream.read_exact(&mut len_buf)).await {
            Ok(Ok(_)) => {},
            Ok(Err(e)) => return Err(anyhow::anyhow!("Failed to read message length: {}", e)),
            Err(_) => return Err(anyhow::anyhow!("Read message timed out")),
        }

        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 {
            // Keep-alive message, ignore
            continue;
        }

        // Read message ID
        let mut id_buf = [0u8; 1];
        if let Err(e) = stream.read_exact(&mut id_buf).await {
            return Err(anyhow::anyhow!("Failed to read message ID: {}", e));
        }

        let id = id_buf[0];

        // Handle extended message
        if id == 20 {
            // Read extended message ID
            let mut ext_id_buf = [0u8; 1];
            if let Err(e) = stream.read_exact(&mut ext_id_buf).await {
                return Err(anyhow::anyhow!("Failed to read extended message ID: {}", e));
            }

            let ext_id = ext_id_buf[0];

            // Read payload
            let payload_len = len - 2; // Subtract message ID and extended ID
            let mut payload = BytesMut::with_capacity(payload_len);
            payload.resize(payload_len, 0);

            if let Err(e) = stream.read_exact(&mut payload).await {
                return Err(anyhow::anyhow!("Failed to read extended message payload: {}", e));
            }

            // Handle extended handshake
            if ext_id == EXT_HANDSHAKE_ID {
                let ext_handshake: HashMap<Vec<u8>, Value> = match serde_bencode::from_bytes(&payload) {
                    Ok(v) => v,
                    Err(e) => {
                        debug!("Failed to parse extended handshake: {}", e);
                        continue;
                    }
                };

                // Get metadata size
                if let Some(Value::Int(size)) = ext_handshake.get(b"metadata_size".as_slice()) {
                    metadata_size = *size as usize;
                    debug!("Metadata size: {} bytes", metadata_size);

                    if metadata_size == 0 || metadata_size > MAX_METADATA_SIZE {
                        return Err(anyhow::anyhow!("Invalid metadata size: {}", metadata_size));
                    }
                }

                // Get ut_metadata ID
                if let Some(Value::Dict(m)) = ext_handshake.get(b"m".as_slice()) {
                    if let Some(Value::Int(id)) = m.get(b"ut_metadata".as_slice()) {
                        ut_metadata_id = *id as u8;
                        debug!("ut_metadata ID: {}", ut_metadata_id);
                    }
                }

                // If we have metadata size and ut_metadata ID, start requesting pieces
                if metadata_size > 0 && ut_metadata_id > 0 {
                    // Calculate number of pieces
                    let num_pieces = (metadata_size + METADATA_PIECE_SIZE - 1) / METADATA_PIECE_SIZE;
                    debug!("Requesting {} metadata pieces", num_pieces);

                    // Request all pieces
                    for piece in 0..num_pieces {
                        let mut request = HashMap::new();
                        request.insert(b"msg_type".to_vec(), Value::Int(REQUEST as i64));
                        request.insert(b"piece".to_vec(), Value::Int(piece as i64));

                        let request_encoded = serde_bencode::to_bytes(&request)
                            .context("Failed to encode metadata request")?;

                        // Send request message
                        let mut message = BytesMut::new();
                        message.put_u32(2 + request_encoded.len() as u32); // length prefix
                        message.put_u8(20); // extended message ID
                        message.put_u8(ut_metadata_id); // ut_metadata ID
                        message.put_slice(&request_encoded);

                        if let Err(e) = stream.write_all(&message).await {
                            return Err(anyhow::anyhow!("Failed to send metadata request: {}", e));
                        }
                    }
                }
            }
            // Handle ut_metadata message
            else if ext_id == ut_metadata_id && metadata_size > 0 {
                // Find the bencode dict at the start of the payload
                let mut dict_end = 0;
                for i in 0..payload.len() {
                    if payload[i] == b'e' {
                        // Check if we've found the end of the dictionary
                        if let Ok(_) = serde_bencode::from_bytes::<HashMap<Vec<u8>, Value>>(&payload[0..i+1]) {
                            dict_end = i + 1;
                            break;
                        }
                    }
                }

                if dict_end == 0 {
                    debug!("Failed to find bencode dict in ut_metadata message");
                    continue;
                }

                // Parse the dict
                let dict: HashMap<Vec<u8>, Value> = match serde_bencode::from_bytes(&payload[0..dict_end]) {
                    Ok(v) => v,
                    Err(e) => {
                        debug!("Failed to parse ut_metadata message: {}", e);
                        continue;
                    }
                };

                // Get message type
                let msg_type = match dict.get(b"msg_type".as_slice()) {
                    Some(Value::Int(t)) => *t as u8,
                    _ => {
                        debug!("Missing msg_type in ut_metadata message");
                        continue;
                    }
                };

                // Get piece index
                let piece = match dict.get(b"piece".as_slice()) {
                    Some(Value::Int(p)) => *p as usize,
                    _ => {
                        debug!("Missing piece in ut_metadata message");
                        continue;
                    }
                };

                match msg_type {
                    DATA => {
                        // Get the data (follows the bencoded dict)
                        let data = BytesMut::from(&payload[dict_end..]);
                        debug!("Received metadata piece {} ({} bytes)", piece, data.len());

                        // Store the piece
                        metadata_pieces.insert(piece, data);

                        // Check if we have all pieces
                        let num_pieces = (metadata_size + METADATA_PIECE_SIZE - 1) / METADATA_PIECE_SIZE;
                        if metadata_pieces.len() == num_pieces {
                            debug!("Received all metadata pieces");

                            // Assemble the complete metadata
                            let mut complete = BytesMut::with_capacity(metadata_size);
                            for i in 0..num_pieces {
                                if let Some(piece) = metadata_pieces.get(&i) {
                                    complete.put_slice(piece);
                                } else {
                                    return Err(anyhow::anyhow!("Missing metadata piece {}", i));
                                }
                            }

                            // Verify the info hash
                            let mut hasher = Sha1::new();
                            hasher.update(&complete);
                            let result = hasher.finalize();

                            let mut computed_hash = [0u8; 20];
                            computed_hash.copy_from_slice(&result);

                            if computed_hash != info_hash {
                                return Err(anyhow::anyhow!("Metadata info hash mismatch"));
                            }

                            metadata = Some(complete);
                            break;
                        }
                    },
                    REJECT => {
                        debug!("Peer rejected metadata piece {}", piece);
                    },
                    _ => {
                        debug!("Unknown ut_metadata message type: {}", msg_type);
                    }
                }
            }
        }
        // Handle other message types (we can ignore them for metadata fetch)
        else {
            // Skip the message
            let payload_len = len - 1; // Subtract message ID
            let mut payload = BytesMut::with_capacity(payload_len);
            payload.resize(payload_len, 0);

            if let Err(e) = stream.read_exact(&mut payload).await {
                return Err(anyhow::anyhow!("Failed to read message payload: {}", e));
            }
        }

        // Check if we have the metadata
        if metadata.is_some() {
            break;
        }
    }

    // Return the metadata
    match metadata {
        Some(m) => Ok(m),
        None => Err(anyhow::anyhow!("Failed to fetch metadata from peer")),
    }
}