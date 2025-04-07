//! Torrent metadata handling

use anyhow::{Context, Result};
use log::{debug, info};
use serde::{Deserialize, Serialize};
use serde_bencode::value::Value;
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

    /// Create metadata from a magnet link (needs to fetch metadata from peers)
    pub async fn from_magnet(info_hash: [u8; 20], name: &str) -> Result<Self> {
        // This would require a more complex implementation to fetch metadata from peers
        // For now, we'll just create a placeholder
        Err(anyhow::anyhow!("Fetching metadata from magnet links not implemented yet"))
    }
}