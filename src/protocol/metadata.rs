//! Torrent metadata handling

use anyhow::{Context, Result};
use log::{debug, info, warn, error};
use serde::{Deserialize, Serialize};
use serde_bencode::value::Value;
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::net::SocketAddr;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use tokio::net::TcpStream;
use std::io::Cursor;
use rand::Rng;

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
    pub async fn from_magnet(info_hash: [u8; 20], name: &str) -> Result<Self> {
        // Constants for metadata exchange
        const METADATA_PIECE_SIZE: usize = 16384; // 16 KiB
        const MAX_METADATA_SIZE: usize = 10 * 1024 * 1024; // 10 MiB
        const METADATA_FETCH_TIMEOUT: u64 = 60; // 60 seconds
        const MAX_PEERS: usize = 10;

        info!("Fetching metadata for magnet link with info hash: {:?}", info_hash);

        // Generate a random peer ID (20 bytes)
        let mut peer_id = [0u8; 20];
        let mut rng = rand::thread_rng();
        rng.fill(&mut peer_id[..]);

        // In a real implementation, we would fetch peers from trackers or DHT
        // For this implementation, we'll connect to a predefined list of peers for the demo
        // This would be provided from the magnet link's trackers in the real world

        // Function to connect to a single peer and fetch metadata
        async fn fetch_from_peer(
            peer_addr: SocketAddr,
            info_hash: [u8; 20],
            peer_id: [u8; 20]
        ) -> Result<BytesMut> {
            debug!("Connecting to peer {} for metadata", peer_addr);

            // Connect to peer with timeout
            let stream = timeout(
                Duration::from_secs(30),
                TcpStream::connect(peer_addr)
            ).await??;

            let (mut reader, mut writer) = stream.into_split();

            // Send handshake with extension protocol support
            let mut handshake = BytesMut::with_capacity(68);
            handshake.put_u8(19);
            handshake.put_slice(b"BitTorrent protocol");

            // Enable extension protocol in reserved bytes
            let mut reserved = [0u8; 8];
            reserved[5] |= 0x10;  // Extension protocol flag
            handshake.put_slice(&reserved);

            handshake.put_slice(&info_hash);
            handshake.put_slice(&peer_id);
            writer.write_all(&handshake).await?;

            // Read handshake response
            let mut response = [0u8; 68];
            reader.read_exact(&mut response).await?;

            // Verify handshake
            if response[0] != 19 || &response[1..20] != b"BitTorrent protocol" {
                return Err(anyhow::anyhow!("Invalid handshake response"));
            }

            // Check if extension protocol is supported
            if (response[25] & 0x10) == 0 {
                return Err(anyhow::anyhow!("Peer doesn't support extension protocol"));
            }

            // Send extension handshake
            let extension_data = serde_json::json!({
                "m": {
                    "ut_metadata": 1
                },
                "v": "SweetMagnet/1.0",
                "metadata_size": 0
            });

            let ext_data = serde_json::to_vec(&extension_data)?;
            let mut ext_msg = BytesMut::with_capacity(ext_data.len() + 6);
            ext_msg.put_u32((ext_data.len() as u32) + 2);
            ext_msg.put_u8(20); // Extension message ID
            ext_msg.put_u8(0);  // Handshake ID
            ext_msg.put_slice(&ext_data);
            writer.write_all(&ext_msg).await?;

            // Process messages to get metadata
            let mut metadata_size = 0;
            let mut ut_metadata_id = 0;
            let mut pieces = HashMap::new();
            let mut num_pieces = 0;

            // Main message processing loop
            loop {
                // Read message length
                let mut len_buf = [0u8; 4];
                reader.read_exact(&mut len_buf).await?;
                let msg_len = u32::from_be_bytes(len_buf);

                if msg_len == 0 {
                    // Keep-alive message, ignore
                    continue;
                }

                // Read message ID
                let mut id_buf = [0u8; 1];
                reader.read_exact(&mut id_buf).await?;
                let msg_id = id_buf[0];

                if msg_id == 20 { // Extension message
                    // Read extension ID
                    let mut ext_id_buf = [0u8; 1];
                    reader.read_exact(&mut ext_id_buf).await?;
                    let ext_id = ext_id_buf[0];

                    // Read rest of the message
                    let payload_len = msg_len as usize - 2;
                    let mut data = vec![0u8; payload_len];
                    reader.read_exact(&mut data).await?;

                    if ext_id == 0 { // Extension handshake
                        if let Ok(response) = serde_json::from_slice::<serde_json::Value>(&data) {
                            // Extract metadata size
                            if let Some(size) = response.get("metadata_size").and_then(|v| v.as_u64()) {
                                metadata_size = size as usize;
                                num_pieces = (metadata_size + METADATA_PIECE_SIZE - 1) / METADATA_PIECE_SIZE; // Ceiling division
                                debug!("Metadata size: {} bytes, {} pieces", metadata_size, num_pieces);
                            }

                            // Extract ut_metadata extension ID
                            if let Some(m) = response.get("m").and_then(|v| v.as_object()) {
                                if let Some(id) = m.get("ut_metadata").and_then(|v| v.as_u64()) {
                                    ut_metadata_id = id as u8;
                                    debug!("ut_metadata extension ID: {}", ut_metadata_id);

                                    // Request all metadata pieces
                                    for piece in 0..num_pieces {
                                        let request = serde_json::json!({
                                            "msg_type": 0, // request
                                            "piece": piece
                                        });

                                        let req_data = serde_json::to_vec(&request)?;
                                        let mut req_msg = BytesMut::with_capacity(req_data.len() + 6);
                                        req_msg.put_u32((req_data.len() as u32) + 2);
                                        req_msg.put_u8(20); // Extension message ID
                                        req_msg.put_u8(ut_metadata_id); // ut_metadata ID
                                        req_msg.put_slice(&req_data);
                                        writer.write_all(&req_msg).await?;
                                    }
                                }
                            }
                        }
                    } else if ext_id == ut_metadata_id {
                        // Process metadata piece message
                        // Find the bencoded dictionary at the beginning
                        let mut dict_end = 0;
                        for i in 0..data.len().min(100) { // Look in first 100 bytes
                            if data[i] == b'e' && i + 1 < data.len() &&
                                (data[i+1] == b'0' || data[i+1] == b'1' || data[i+1] == b'2') {
                                dict_end = i + 1;
                                break;
                            }
                        }

                        if dict_end > 0 {
                            if let Ok(piece_info) = serde_json::from_slice::<serde_json::Value>(&data[..dict_end]) {
                                // Check message type
                                if let Some(msg_type) = piece_info.get("msg_type").and_then(|v| v.as_u64()) {
                                    if msg_type == 1 { // Data
                                        if let Some(piece) = piece_info.get("piece").and_then(|v| v.as_u64()) {
                                            let piece_index = piece as usize;
                                            let piece_data = Bytes::copy_from_slice(&data[dict_end..]);

                                            // Save the piece
                                            pieces.insert(piece_index, piece_data);
                                            debug!("Received metadata piece {}/{}", piece_index + 1, num_pieces);

                                            // Check if we have all pieces
                                            if pieces.len() == num_pieces {
                                                // Combine all pieces
                                                let mut complete_metadata = BytesMut::with_capacity(metadata_size);
                                                for i in 0..num_pieces {
                                                    if let Some(piece_data) = pieces.get(&i) {
                                                        complete_metadata.extend_from_slice(piece_data);
                                                    }
                                                }

                                                // Truncate to the correct size (last piece might be padded)
                                                if complete_metadata.len() > metadata_size {
                                                    complete_metadata.truncate(metadata_size);
                                                }

                                                return Ok(complete_metadata);
                                            }
                                        }
                                    } else if msg_type == 2 { // Reject
                                        warn!("Peer rejected metadata request");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // In a real implementation, we would get peers from trackers or DHT
        // For now, we'll create a simulated list of peers for demonstration
        let peers = Vec::<SocketAddr>::new(); // This would be populated from trackers

        if peers.is_empty() {
            return Err(anyhow::anyhow!("No peers available to fetch metadata"));
        }

        // Try to fetch from multiple peers concurrently
        let metadata_result = timeout(
            Duration::from_secs(METADATA_FETCH_TIMEOUT),
            async {
                let mut tasks = Vec::new();

                // Start concurrent tasks for each peer (limit to MAX_PEERS)
                for &peer_addr in peers.iter().take(MAX_PEERS) {
                    let info_hash_clone = info_hash;
                    let peer_id_clone = peer_id;

                    let task = tokio::spawn(async move {
                        match fetch_from_peer(peer_addr, info_hash_clone, peer_id_clone).await {
                            Ok(metadata) => Some(metadata),
                            Err(e) => {
                                debug!("Failed to fetch metadata from {}: {}", peer_addr, e);
                                None
                            }
                        }
                    });

                    tasks.push(task);
                }

                // Wait for the first successful result
                for task in tasks {
                    if let Ok(Some(metadata)) = task.await {
                        // We got the metadata from one peer, abort other tasks
                        return Ok(metadata);
                    }
                }

                Err(anyhow::anyhow!("Failed to fetch metadata from any peer"))
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