//! Piece management and download tracking

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use log::{debug, error, info, warn};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::Write;
use std::path::Path;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// A block of data within a piece
#[derive(Debug, Clone)]
pub struct Block {
    pub piece_index: usize,
    pub offset: usize,
    pub length: usize,
    pub data: Option<Bytes>,
    pub requested: bool,
    pub received: bool,
    pub request_time: Option<std::time::Instant>,
}

impl Block {
    pub fn new(piece_index: usize, offset: usize, length: usize) -> Self {
        Block {
            piece_index,
            offset,
            length,
            data: None,
            requested: false,
            received: false,
            request_time: None,
        }
    }
}

/// A piece of the torrent
#[derive(Debug)]
pub struct Piece {
    pub index: usize,
    pub hash: [u8; 20],
    pub length: usize,
    pub blocks: Vec<Block>,
    pub downloaded: bool,
    pub verified: bool,
    pub priority: usize, // Higher means more important
}

impl Piece {
    /// Create a new piece with blocks
    pub fn new(index: usize, hash: [u8; 20], length: usize, block_size: usize) -> Self {
        let mut blocks = Vec::new();
        let mut offset = 0;

        while offset < length {
            let block_length = std::cmp::min(block_size, length - offset);
            blocks.push(Block::new(index, offset, block_length));
            offset += block_length;
        }

        Piece {
            index,
            hash,
            length,
            blocks,
            downloaded: false,
            verified: false,
            priority: 1, // Default priority
        }
    }

    /// Check if the piece is complete (all blocks received)
    pub fn is_complete(&self) -> bool {
        self.blocks.iter().all(|block| block.received)
    }

    /// Get the next block that needs to be requested
    pub fn next_block_to_request(&mut self) -> Option<&mut Block> {
        self.blocks.iter_mut().find(|block| !block.requested && !block.received)
    }

    /// Get blocks that have been requested but not received and timed out
    pub fn get_timed_out_blocks(&mut self, timeout: std::time::Duration) -> Vec<(usize, usize)> {
        let now = std::time::Instant::now();

        self.blocks
            .iter_mut()
            .filter_map(|block| {
                if block.requested &&
                    !block.received &&
                    block.request_time.map_or(false, |time| now.duration_since(time) > timeout) {
                    // Return the offset of the timed out block
                    Some((self.index, block.offset))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Reset a block to be requested again
    pub fn reset_block(&mut self, block_offset: usize) {
        if let Some(block) = self.blocks.iter_mut().find(|b| b.offset == block_offset) {
            block.requested = false;
            block.request_time = None;
        }
    }

    /// Add received block data
    pub fn add_block_data(&mut self, offset: usize, data: Bytes) -> bool {
        if let Some(block) = self.blocks.iter_mut().find(|b| b.offset == offset) {
            if !block.received {
                block.data = Some(data);
                block.received = true;
                return true;
            }
        }
        false
    }

    /// Verify the piece hash
    pub fn verify(&mut self) -> bool {
        // Only verify if all blocks are received
        if !self.is_complete() {
            return false;
        }

        // Concatenate all block data
        let mut hasher = Sha1::new();
        let mut data = Vec::with_capacity(self.length);

        for block in &self.blocks {
            if let Some(block_data) = &block.data {
                data.extend_from_slice(block_data);
            } else {
                // This shouldn't happen if is_complete() returned true
                return false;
            }
        }

        // Calculate hash
        hasher.update(&data);
        let result = hasher.finalize();

        // Compare with expected hash
        let verified = result[..] == self.hash;

        if verified {
            self.verified = true;
            self.downloaded = true;
            debug!("Piece {} verified successfully", self.index);
        } else {
            warn!("Piece {} verification failed", self.index);
            // Reset all blocks
            for block in self.blocks.iter_mut() {
                block.requested = false;
                block.received = false;
                block.data = None;
                block.request_time = None;
            }
        }

        verified
    }

    /// Write piece data to a file
    pub async fn write_to_file(&self, file_path: &Path, offset: u64) -> Result<()> {
        if !self.verified {
            return Err(anyhow::anyhow!("Cannot write unverified piece to file"));
        }

        // Open file for writing
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(file_path)
            .await
            .context("Failed to open file for writing")?;

        // Seek to the correct position
        tokio::io::AsyncSeekExt::seek(
            &mut file,
            std::io::SeekFrom::Start(offset)
        ).await
            .context("Failed to seek in file")?;

        // Write each block in order
        for block in &self.blocks {
            if let Some(data) = &block.data {
                file.write_all(data).await
                    .context("Failed to write block data to file")?;
            } else {
                return Err(anyhow::anyhow!("Missing block data in verified piece"));
            }
        }

        file.flush().await
            .context("Failed to flush file")?;

        Ok(())
    }
}

/// Manages the downloading of pieces
pub struct PieceManager {
    pub pieces: Vec<Piece>,
    pub downloaded_pieces: HashSet<usize>,
    pub needed_pieces: VecDeque<usize>,
    pub in_progress: HashSet<usize>,
    pub block_size: usize,
    pub piece_request_timeout: std::time::Duration,
}

impl PieceManager {
    /// Create a new piece manager
    pub fn new(
        piece_hashes: Vec<[u8; 20]>,
        piece_lengths: Vec<usize>,
        block_size: usize,
    ) -> Self {
        let mut pieces = Vec::with_capacity(piece_hashes.len());
        let mut needed_pieces = VecDeque::with_capacity(piece_hashes.len());

        for (i, (hash, length)) in piece_hashes.iter().zip(piece_lengths.iter()).enumerate() {
            pieces.push(Piece::new(i, *hash, *length, block_size));
            needed_pieces.push_back(i);
        }

        PieceManager {
            pieces,
            downloaded_pieces: HashSet::new(),
            needed_pieces,
            in_progress: HashSet::new(),
            block_size,
            piece_request_timeout: std::time::Duration::from_secs(30),
        }
    }

    /// Get the next piece to download
    pub fn next_piece(&mut self) -> Option<&mut Piece> {
        // Find a piece that isn't in progress
        while let Some(index) = self.needed_pieces.pop_front() {
            if !self.in_progress.contains(&index) && !self.downloaded_pieces.contains(&index) {
                self.in_progress.insert(index);
                return self.pieces.get_mut(index);
            }
        }
        None
    }

    /// Handle a received block
    pub fn received_block(&mut self, piece_index: usize, offset: usize, data: Bytes) -> bool {
        if let Some(piece) = self.pieces.get_mut(piece_index) {
            if piece.add_block_data(offset, data) {
                if piece.is_complete() {
                    if piece.verify() {
                        self.downloaded_pieces.insert(piece_index);
                        self.in_progress.remove(&piece_index);
                        return true;
                    } else {
                        // Verification failed, add back to needed pieces
                        self.in_progress.remove(&piece_index);
                        self.needed_pieces.push_back(piece_index);
                    }
                }
            }
        }
        false
    }

    /// Check for timed out piece requests and reset them
    pub fn check_timeouts(&mut self) {
        // Copy the set to avoid borrowing issues
        let in_progress = self.in_progress.clone();

        for index in in_progress {
            if let Some(piece) = self.pieces.get_mut(index) {
                // Get timed out blocks as (piece_index, block_offset) tuples
                let timed_out_blocks = piece.get_timed_out_blocks(self.piece_request_timeout);

                // Check if we have any timed out blocks
                if !timed_out_blocks.is_empty() {
                    // Process each timed out block
                    for (piece_index, block_offset) in timed_out_blocks {
                        // Reset the block
                        if let Some(piece) = self.pieces.get_mut(piece_index) {
                            piece.reset_block(block_offset);
                            debug!("Block request timed out: piece {} offset {}",
                                   piece_index, block_offset);
                        }
                    }

                    // Check if all blocks are reset
                    if let Some(piece) = self.pieces.get(index) {
                        if piece.blocks.iter().all(|b| !b.requested && !b.received) {
                            self.in_progress.remove(&index);
                            self.needed_pieces.push_back(index);
                        }
                    }
                }
            }
        }
    }

    /// Get download progress as a percentage
    pub fn download_progress(&self) -> f64 {
        if self.pieces.is_empty() {
            return 0.0;
        }

        (self.downloaded_pieces.len() as f64 / self.pieces.len() as f64) * 100.0
    }

    /// Write all downloaded pieces to a file
    pub async fn write_to_file(&self, file_path: &Path) -> Result<()> {
        debug!("Writing downloaded pieces to file: {}", file_path.display());

        // Ensure parent directory exists
        if let Some(parent) = file_path.parent() {
            tokio::fs::create_dir_all(parent).await
                .context("Failed to create directory")?;
        }

        // Create or truncate the output file
        let file = File::create(file_path).await
            .context("Failed to create output file")?;

        // Close the file handle for now
        drop(file);

        let mut offset = 0;
        for piece in &self.pieces {
            if piece.verified {
                piece.write_to_file(file_path, offset).await
                    .context(format!("Failed to write piece {} to file", piece.index))?;
            }
            offset += piece.length as u64;
        }

        Ok(())
    }
}