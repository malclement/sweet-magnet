// src/torrent/client.rs
use crate::error::SweetMagnetError;
use crate::protocol::{
    PeerConnection, PeerEvent, PieceManager, Tracker, TrackerEvent, TorrentMetadata, TrackerOperationEvent,
};
use crate::torrent::magnet::MagnetLink;
use anyhow::{Context, Result};
use bytes::Bytes;
use log::{debug, error, info, trace, warn};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::sync::{mpsc, Mutex};
use tokio::time::interval;

const BLOCK_SIZE: usize = 16384; // 16 KiB
const MAX_CONNECTIONS: usize = 50;
const TRACKER_INTERVAL: u64 = 1800; // 30 minutes
const PEER_TIMEOUT: u64 = 120; // 2 minutes
const DEFAULT_METADATA_TIMEOUT: u64 = 60; // 1 minute

/// Torrent client stats
#[derive(Debug, Clone, Default)]
pub struct ClientStats {
    pub downloaded: u64,
    pub uploaded: u64,
    pub download_speed: f64, // bytes per second
    pub upload_speed: f64,   // bytes per second
    pub num_peers: usize,
    pub progress: f64, // percentage
    pub elapsed_time: Duration,
}

impl ClientStats {
    fn new() -> Self {
        ClientStats {
            downloaded: 0,
            uploaded: 0,
            download_speed: 0.0,
            upload_speed: 0.0,
            num_peers: 0,
            progress: 0.0,
            elapsed_time: Duration::default(),
        }
    }
}

/// Basic torrent client implementation
pub struct TorrentClient {
    magnet: MagnetLink,
    download_dir: PathBuf,
    peer_id: [u8; 20],
    info_hash: [u8; 20],
    trackers: Vec<Tracker>,
    metadata: Option<TorrentMetadata>,
    piece_manager: Option<PieceManager>,
    active_peers: HashMap<SocketAddr, PeerConnection>,
    stats: ClientStats,
    start_time: Instant,
    connection_timeout: Duration,
}

impl TorrentClient {
    /// Create a new torrent client instance
    pub fn new(magnet: MagnetLink, download_dir: &str) -> Self {
        // Create a random peer ID (20 bytes as per BitTorrent spec)
        // Here we use a simple format: -SM0001-xxxxxxxxxxxx (SM = SweetMagnet)
        let mut peer_id = [0u8; 20];
        let prefix = b"-SM0001-";
        peer_id[..8].copy_from_slice(prefix);

        // Fill the rest with random bytes
        for i in 8..20 {
            peer_id[i] = rand::random::<u8>();
        }

        // Convert info hash from hex to bytes
        let mut info_hash = [0u8; 20];
        for i in 0..20 {
            let byte_str = &magnet.info_hash[i*2..i*2+2];
            info_hash[i] = u8::from_str_radix(byte_str, 16).unwrap_or(0);
        }

        // Create trackers
        let mut trackers = Vec::new();
        for tracker_url in &magnet.trackers {
            let tracker = Tracker::new(
                tracker_url,
                info_hash,
                peer_id,
                0, // We don't know the total size yet
            );
            trackers.push(tracker);
        }

        TorrentClient {
            magnet,
            download_dir: PathBuf::from(download_dir),
            peer_id,
            info_hash,
            trackers,
            metadata: None,
            piece_manager: None,
            active_peers: HashMap::new(),
            stats: ClientStats::new(),
            start_time: Instant::now(),
            connection_timeout: Duration::from_secs(30),
        }
    }

    /// Initialize the client and prepare for downloading
    pub async fn initialize(&mut self) -> Result<(), SweetMagnetError> {
        info!("Initializing torrent client for: {}", self.magnet.name());

        // Ensure download directory exists
        if !self.download_dir.exists() {
            fs::create_dir_all(&self.download_dir)
                .await
                .map_err(SweetMagnetError::from)?;
        }

        info!("Using download directory: {}", self.download_dir.display());
        debug!("Using peer ID: {:?}", self.peer_id);

        // Connect to trackers to get peer information
        self.connect_to_trackers().await?;

        Ok(())
    }

    /// Connect to trackers to get peer information
    async fn connect_to_trackers(&mut self) -> Result<(), SweetMagnetError> {
        if self.trackers.is_empty() {
            warn!("No trackers specified in magnet link");
            return Ok(());
        }

        // Create channel for tracker operation events
        let (tx, mut rx) = mpsc::channel::<TrackerOperationEvent>(100);

        let mut pending_trackers = self.trackers.len();
        let mut all_peers = HashSet::new();
        let mut successful_trackers = 0;

        // Start contacting all trackers in parallel
        for tracker in &mut self.trackers {
            let tracker_tx = tx.clone();
            let mut tracker_clone = tracker.clone();

            // Spawn task for each tracker
            tokio::spawn(async move {
                let _ = tracker_clone.announce_with_events(TrackerEvent::Started, tracker_tx).await;
            });
        }

        // Drop the original sender so the channel can close when all tasks are done
        drop(tx);

        // Process tracker events
        while let Some(event) = rx.recv().await {
            match event {
                TrackerOperationEvent::Connected(url, peer_count) => {
                    info!("Connected to tracker: {} - Found {} peers", url, peer_count);
                    successful_trackers += 1;
                },
                TrackerOperationEvent::Response(response) => {
                    // Add peers to our set
                    all_peers.extend(response.peers);
                },
                TrackerOperationEvent::Failed(url, reason) => {
                    warn!("Failed to connect to tracker {}: {}", url, reason);
                    pending_trackers -= 1;
                },
                TrackerOperationEvent::Error(url, message) => {
                    warn!("Tracker {} error: {}", url, message);
                    pending_trackers -= 1;
                },
                _ => {}
            }
        }

        if successful_trackers == 0 {
            return Err(SweetMagnetError::TorrentClient(
                "Failed to connect to any trackers".to_string(),
            ));
        }

        info!("Found {} unique peers from {} trackers", all_peers.len(), successful_trackers);

        Ok(())
    }

    /// Set up a background task to periodically announce to trackers
    async fn start_tracker_announcer(&self) -> mpsc::Receiver<TrackerOperationEvent> {
        let (tx, rx) = mpsc::channel::<TrackerOperationEvent>(100);

        // Clone what we need for the background task
        let trackers = self.trackers.clone();
        let downloaded = self.stats.downloaded;
        let uploaded = self.stats.uploaded;
        let info_hash = self.info_hash;

        // Start background task
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(TRACKER_INTERVAL));

            loop {
                interval.tick().await;

                for mut tracker in trackers.clone() {
                    // Update tracker with latest stats
                    tracker.update_stats(uploaded, downloaded, 0); // 0 = left (complete)

                    // Announce with the Empty event (regular announce)
                    let tracker_tx = tx.clone();

                    tokio::spawn(async move {
                        if let Err(e) = tracker.announce_with_events(TrackerEvent::Empty, tracker_tx).await {
                            // Error already reported through the channel
                            debug!("Tracker announce error: {}", e);
                        }
                    });
                }
            }
        });

        rx
    }

    /// Start downloading the torrent
    pub async fn download(&mut self) -> Result<String, SweetMagnetError> {
        info!("Starting download for: {}", self.magnet.name());

        // Set up channels for peer events
        let (events_tx, mut events_rx) = mpsc::channel::<PeerEvent>(100);

        // Try to connect to some initial peers
        let mut connected_peers = 0;

        // Collect peer addresses first to avoid the borrow conflict
        let mut peer_addresses = Vec::new();
        for tracker in &self.trackers {
            if let Ok(response) = tracker.announce(TrackerEvent::Started).await {
                for peer_addr in response.peers.iter().take(MAX_CONNECTIONS) {
                    peer_addresses.push(*peer_addr);
                }
            }
        }

        // Now connect to peers without borrowing self.trackers
        for peer_addr in peer_addresses {
            if self.connect_to_peer(peer_addr, events_tx.clone()).await.is_ok() {
                connected_peers += 1;
                if connected_peers >= MAX_CONNECTIONS {
                    break;
                }
            }
        }

        if connected_peers == 0 {
            return Err(SweetMagnetError::TorrentClient(
                "Could not connect to any peers".to_string(),
            ));
        }

        info!("Connected to {} peers initially", connected_peers);

        // Wait for metadata if we don't have it yet
        if self.metadata.is_none() {
            info!("Waiting for torrent metadata...");

            // In a real implementation, we would fetch metadata from peers (BEP-9)
            // For simplicity here, we'll just create a placeholder file

            let file_path = self.download_dir.join(format!(
                "{}.torrent",
                self.magnet.name().replace(' ', "_")
            ));

            // Create a simple metadata file
            let mut file = tokio::fs::File::create(&file_path)
                .await
                .map_err(SweetMagnetError::from)?;

            // Return early for now - in a real implementation we'd continue with download
            return Ok(file_path.to_string_lossy().to_string());
        }

        // Main download loop
        let mut stats_interval = interval(Duration::from_secs(1));
        let mut active_requests = HashMap::new();

        // This block of code has a logical error - we're trying to loop through connected_peers
        // but connected_peers is just a counter. Commenting out this block for now.
        /*
        for peer_addr in connected_peers {
            if self.connect_to_peer(peer_addr, events_tx.clone()).await.is_ok() {
                if self.active_peers.len() >= MAX_CONNECTIONS {
                    break;
                }
            }
        }
        */

        loop {
            tokio::select! {
                Some(event) = events_rx.recv() => {
                    self.handle_peer_event(event, &mut active_requests).await?;
                }
                _ = stats_interval.tick() => {
                    self.update_stats();

                    // Print progress
                    if let Some(piece_manager) = &self.piece_manager {
                        let progress = piece_manager.download_progress();
                        info!("Download progress: {:.2}% | Speed: {:.2} KB/s | Connected peers: {}",
                              progress,
                              self.stats.download_speed / 1024.0,
                              self.active_peers.len());

                        // Check if download is complete
                        if progress >= 100.0 {
                            info!("Download completed!");

                            // Write the file
                            let output_path = self.download_dir.join(&self.magnet.name());
                            self.save_download(&output_path).await?;

                            return Ok(output_path.to_string_lossy().to_string());
                        }
                    }

                    // Check timeouts and request more pieces
                    self.check_timeouts_and_request_pieces(&mut active_requests).await?;
                }
            }
        }
    }

    /// Connect to a peer
    async fn connect_to_peer(
        &mut self,
        addr: SocketAddr,
        events_tx: mpsc::Sender<PeerEvent>,
    ) -> Result<()> {
        debug!("Connecting to peer at {}", addr);

        if self.active_peers.contains_key(&addr) {
            return Ok(());
        }

        // Create a connection
        let mut connection = PeerConnection::new(
            addr,
            self.info_hash,
            self.peer_id,
            1000, // Placeholder for total pieces
            events_tx.clone(),
            self.connection_timeout,
        ).await?;

        // Store it before moving it
        self.active_peers.insert(addr, connection);
        self.stats.num_peers = self.active_peers.len();

        // Get a clone of what we need for the task
        let events_tx_clone = events_tx.clone();
        let conn_addr = addr;

        // Start the connection (handshake, etc.) in a separate task
        tokio::spawn(async move {
            // Get a reference to the connection from active_peers in a new task
            // You'll need to modify PeerConnection to add a run_detached method
            if let Err(e) = PeerConnection::run_detached(conn_addr, events_tx_clone.clone()).await {
                error!("Peer connection error: {}", e);
                if let Err(e) = events_tx_clone
                    .send(PeerEvent::Disconnected(conn_addr, Some(e.to_string())))
                    .await
                {
                    error!("Failed to send disconnection event: {}", e);
                }
            }
        });

        Ok(())
    }

    /// Handle a peer event
    async fn handle_peer_event(
        &mut self,
        event: PeerEvent,
        active_requests: &mut HashMap<(usize, usize), Instant>,
    ) -> Result<(), SweetMagnetError> {
        match event {
            PeerEvent::Connected(addr) => {
                debug!("Peer connection established: {}", addr);
                // Peer is ready to communicate, we can start requesting pieces
                if let Some(peer) = self.active_peers.get_mut(&addr) {
                    // In a full implementation, we'd send interested here if needed
                }
            }
            PeerEvent::Bitfield(addr, pieces) => {
                debug!("Received bitfield from peer {} with {} pieces", addr, pieces.iter().filter(|&&has_piece| has_piece).count());

                // Update our knowledge of which pieces the peer has
                // In a real implementation, we would select pieces to download based on this
                if let Some(piece_manager) = &mut self.piece_manager {
                    // Use bitfield to determine which pieces to request from this peer
                    // This would involve determining rare pieces and prioritizing them
                }
            }
            PeerEvent::Have(addr, piece_index) => {
                debug!("Peer {} has piece {}", addr, piece_index);

                // Update the peer's available pieces
                // In a full implementation, we would use this to request pieces
                if let Some(piece_manager) = &mut self.piece_manager {
                    // If it's a piece we need, consider requesting it
                }
            }
            PeerEvent::Choke(addr) => {
                debug!("Peer {} choked us", addr);

                // We can't request pieces from this peer anymore
                // Cancel any pending requests to this peer
                if let Some(peer_conn) = self.active_peers.get(&addr) {
                    // Find all active requests for this peer and remove them
                    let to_remove: Vec<_> = active_requests
                        .iter()
                        .filter(|_| true) // In a real impl, filter requests for this peer
                        .map(|((piece, offset), _)| (*piece, *offset))
                        .collect();

                    for (piece, offset) in to_remove {
                        active_requests.remove(&(piece, offset));
                    }
                }
            }
            PeerEvent::Unchoke(addr) => {
                debug!("Peer {} unchoked us", addr);

                // We can now request pieces from this peer
                // In a real implementation, we'd immediately request some pieces
                if let Some(peer_conn) = self.active_peers.get_mut(&addr) {
                    // Request pieces if we're interested
                    if let Some(piece_manager) = &mut self.piece_manager {
                        // Request next piece logic would go here
                    }
                }
            }
            PeerEvent::Interested(addr) => {
                debug!("Peer {} is interested in our pieces", addr);
                // In a full implementation with uploading, we'd decide whether to unchoke
            }
            PeerEvent::NotInterested(addr) => {
                debug!("Peer {} is not interested in our pieces", addr);
                // No action needed typically
            }
            PeerEvent::BlockReceived(addr, piece_index, offset, data) => {
                trace!("Received block from peer {}: piece={}, offset={}, size={}",
                      addr, piece_index, offset, data.len());

                // Remove from active requests
                active_requests.remove(&(piece_index, offset));

                // Process the received data
                if let Some(piece_manager) = &mut self.piece_manager {
                    // Add the block to our piece manager
                    let complete = piece_manager.received_block(piece_index, offset, data.clone());

                    if complete {
                        info!("Completed piece {}", piece_index);

                        // If the piece was verified, request more pieces
                        // In a complete implementation, we would also announce to peers
                        // that we have this piece using "have" messages
                    }
                }

                // Update stats
                self.stats.downloaded += data.len() as u64;
            }
            PeerEvent::BlockRequested(addr, piece_index, offset, length) => {
                debug!("Peer {} requested piece {} offset {} length {}",
                      addr, piece_index, offset, length);

                // In a full implementation with uploading, we'd respond with the requested data
                // if we have it and aren't choking the peer
            }
            PeerEvent::KeepAlive(addr) => {
                trace!("Received keep-alive from peer {}", addr);
                // No action needed for keep-alive messages
            }
            PeerEvent::Error(addr, error) => {
                warn!("Error from peer {}: {}", addr, error);
                // Depending on the error, we might want to disconnect or retry
            }
            PeerEvent::Disconnected(addr, reason) => {
                if let Some(reason) = reason {
                    debug!("Peer {} disconnected: {}", addr, reason);
                } else {
                    debug!("Peer {} disconnected", addr);
                }

                // Clean up peer connection
                self.active_peers.remove(&addr);
                self.stats.num_peers = self.active_peers.len();

                // Connect to a new peer if we have too few connections
                if self.active_peers.len() < MAX_CONNECTIONS / 2 {
                    // In a real implementation, we'd try to connect to new peers here
                    // to maintain a good number of connections
                }
            }
        }

        Ok(())
    }

    /// Update client statistics
    fn update_stats(&mut self) {
        let elapsed = self.start_time.elapsed();
        self.stats.elapsed_time = elapsed;

        // Calculate speeds
        if elapsed.as_secs() > 0 {
            self.stats.download_speed = self.stats.downloaded as f64 / elapsed.as_secs() as f64;
            self.stats.upload_speed = self.stats.uploaded as f64 / elapsed.as_secs() as f64;
        }

        // Update progress if we have a piece manager
        if let Some(piece_manager) = &self.piece_manager {
            self.stats.progress = piece_manager.download_progress();
        }
    }

    /// Check for timed out requests and request new pieces
    async fn check_timeouts_and_request_pieces(
        &mut self,
        active_requests: &mut HashMap<(usize, usize), Instant>,
    ) -> Result<(), SweetMagnetError> {
        // Check for timed out requests
        let now = Instant::now();
        let timeout = Duration::from_secs(30);

        let timed_out: Vec<_> = active_requests
            .iter()
            .filter(|(_, time)| now.duration_since(**time) > timeout)
            .map(|((piece, offset), _)| (*piece, *offset))
            .collect();

        for (piece, offset) in timed_out {
            active_requests.remove(&(piece, offset));
            // In a real implementation, we would reset this request in our piece manager
        }

        // In a real implementation, we would request new pieces here

        Ok(())
    }

    /// Save the downloaded content to a file
    async fn save_download(&self, output_path: &Path) -> Result<(), SweetMagnetError> {
        // In a real implementation, we would save all the downloaded pieces to the appropriate files
        // For now we'll just create a placeholder file

        // Ensure parent directory exists
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(SweetMagnetError::from)?;
        }

        // Create a simple file
        let result = fs::write(
            output_path,
            format!(
                "Mock torrent data for {}\nInfo hash: {}\nDownloaded at: {:?}",
                self.magnet.name(),
                self.magnet.info_hash(),
                chrono::Local::now()
            )
        ).await;

        match result {
            Ok(_) => {
                info!("Downloaded file saved to: {}", output_path.display());
                Ok(())
            }
            Err(e) => Err(SweetMagnetError::from(e)),
        }
    }
}

// Helper function to trace log
#[inline]
fn trace<T: std::fmt::Display>(msg: T) {
    if log::log_enabled!(log::Level::Trace) {
        log::trace!("{}", msg);
    }
}