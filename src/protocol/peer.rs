//! Peer connection and management

use crate::protocol::message::{Message, MessageType};
use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use log::{debug, error, info, trace, warn};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;

/// BitTorrent peer information
#[derive(Debug, Clone)]
pub struct Peer {
    pub id: Option<[u8; 20]>,
    pub addr: SocketAddr,
    pub pieces: Vec<bool>,
    pub choked: bool,
    pub interested: bool,
    pub am_choking: bool,
    pub am_interested: bool,
    pub last_seen: Instant,
}

impl Peer {
    /// Create a new peer from address
    pub fn new(addr: SocketAddr) -> Self {
        Peer {
            id: None,
            addr,
            pieces: Vec::new(),
            choked: true,
            interested: false,
            am_choking: true,
            am_interested: false,
            last_seen: Instant::now(),
        }
    }

    /// Check if peer has a specific piece
    pub fn has_piece(&self, index: usize) -> bool {
        self.pieces.get(index).copied().unwrap_or(false)
    }

    /// Update peer's bitfield
    pub fn update_bitfield(&mut self, bitfield: &[u8], total_pieces: usize) {
        self.pieces = Vec::with_capacity(total_pieces);
        for i in 0..total_pieces {
            let byte_index = i / 8;
            let bit_index = 7 - (i % 8);
            if byte_index < bitfield.len() {
                let has_piece = (bitfield[byte_index] >> bit_index) & 1 == 1;
                self.pieces.push(has_piece);
            } else {
                self.pieces.push(false);
            }
        }
    }

    /// Update when peer has a new piece
    pub fn received_have(&mut self, piece_index: usize) {
        if piece_index >= self.pieces.len() {
            // Resize if needed
            self.pieces.resize(piece_index + 1, false);
        }
        self.pieces[piece_index] = true;
    }
}

/// Messages that can be sent between the peer connection and the torrent client
#[derive(Debug)]
pub enum PeerEvent {
    /// Peer connection has been established (includes peer address)
    Connected(SocketAddr),

    /// Peer sent a bitfield message (includes peer address and bitfield as bool vector)
    Bitfield(SocketAddr, Vec<bool>),

    /// Peer announced it has a piece (includes peer address and piece index)
    Have(SocketAddr, usize),

    /// Peer choked us - we can't request pieces (includes peer address)
    Choke(SocketAddr),

    /// Peer unchoked us - we can request pieces (includes peer address)
    Unchoke(SocketAddr),

    /// Peer is interested in our pieces (includes peer address)
    Interested(SocketAddr),

    /// Peer is not interested in our pieces (includes peer address)
    NotInterested(SocketAddr),

    /// Received a block of data (includes peer address, piece index, offset, and data)
    BlockReceived(SocketAddr, usize, usize, Bytes),

    /// Peer connection closed (includes peer address and optional reason)
    Disconnected(SocketAddr, Option<String>),

    /// Peer requested a block from us (includes peer address, piece index, offset, and length)
    BlockRequested(SocketAddr, usize, usize, usize),

    /// Peer sent a keep-alive message (includes peer address)
    KeepAlive(SocketAddr),

    /// Error occurred with peer (includes peer address and error message)
    Error(SocketAddr, String),
}

/// Manages an active connection to a peer
pub struct PeerConnection {
    peer: Peer,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    total_pieces: usize,
    stream: TcpStream,
    events_tx: mpsc::Sender<PeerEvent>,
    connection_timeout: Duration,
}

impl PeerConnection {
    pub const DEFAULT_BLOCK_SIZE: usize = 16384; // 16 KiB
    pub const MAX_BLOCK_SIZE: usize = 32768; // 32 KiB

    /// Create a new peer connection
    pub async fn new(
        addr: SocketAddr,
        info_hash: [u8; 20],
        peer_id: [u8; 20],
        total_pieces: usize,
        events_tx: mpsc::Sender<PeerEvent>,
        connection_timeout: Duration,
    ) -> Result<Self> {
        debug!("Connecting to peer at {}", addr);

        let stream = timeout(
            connection_timeout,
            TcpStream::connect(addr)
        ).await
            .context("Connection timeout")?
            .context("Failed to connect to peer")?;

        let peer = Peer::new(addr);

        Ok(PeerConnection {
            peer,
            info_hash,
            peer_id,
            total_pieces,
            stream,
            events_tx,
            connection_timeout,
        })
    }

    /// Start the connection process and handle messages
    pub async fn run(&mut self) -> Result<()> {
        // Perform handshake
        self.handshake().await?;

        // Send interested message
        self.send_interested().await?;

        // Notify we're connected
        self.events_tx.send(PeerEvent::Connected(self.peer.addr)).await
            .context("Failed to send connection event")?;

        // Read and process messages
        self.message_loop().await
    }

    /// Perform the BitTorrent handshake
    async fn handshake(&mut self) -> Result<()> {
        debug!("Starting handshake with peer {}", self.peer.addr);

        // Send handshake
        let handshake = Message::new_handshake(&self.info_hash, &self.peer_id);
        handshake.write_to(&mut self.stream).await
            .context("Failed to send handshake")?;

        // Read handshake response
        let mut buf = [0u8; 68];
        timeout(
            self.connection_timeout,
            self.stream.read_exact(&mut buf)
        ).await
            .context("Handshake response timeout")?
            .context("Failed to read handshake response")?;

        // Validate protocol string
        if buf[0] != 19 || &buf[1..20] != b"BitTorrent protocol" {
            return Err(anyhow::anyhow!("Invalid handshake response"));
        }

        // Validate info hash
        if buf[28..48] != self.info_hash {
            return Err(anyhow::anyhow!("Info hash mismatch in handshake"));
        }

        // Extract peer id
        let mut peer_id = [0u8; 20];
        peer_id.copy_from_slice(&buf[48..68]);
        self.peer.id = Some(peer_id);

        debug!("Handshake completed with peer {}", self.peer.addr);
        Ok(())
    }

    /// Main message loop
    async fn message_loop(&mut self) -> Result<()> {
        let mut buf = BytesMut::with_capacity(16384);

        loop {
            // Read message length prefix
            let mut length_buf = [0u8; 4];

            match self.stream.read_exact(&mut length_buf).await {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    // Connection closed
                    debug!("Peer {} disconnected", self.peer.addr);
                    self.events_tx.send(PeerEvent::Disconnected(self.peer.addr, None)).await?;
                    break;
                }
                Err(e) => {
                    // Other error
                    warn!("Error reading from peer {}: {}", self.peer.addr, e);
                    self.events_tx.send(PeerEvent::Disconnected(self.peer.addr, Some(e.to_string()))).await?;
                    return Err(e.into());
                }
            }

            // Parse message length
            let length = u32::from_be_bytes(length_buf);

            // Keep-alive message (length = 0)
            if length == 0 {
                trace!("Received keep-alive from peer {}", self.peer.addr);
                self.peer.last_seen = Instant::now();
                continue;
            }

            // Ensure buffer has enough capacity
            buf.resize(length as usize, 0);

            // Read message body
            self.stream.read_exact(&mut buf).await
                .context("Failed to read message body")?;

            // Process the message
            self.handle_message(&buf).await?;

            // Update last seen time
            self.peer.last_seen = Instant::now();
        }

        Ok(())
    }

    /// Handle a BitTorrent message
    async fn handle_message(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let message_id = data[0];

        match message_id {
            0 => { // Choke
                debug!("Peer {} choked us", self.peer.addr);
                self.peer.choked = true;
                self.events_tx.send(PeerEvent::Choke(self.peer.addr)).await?;
            }
            1 => { // Unchoke
                debug!("Peer {} unchoked us", self.peer.addr);
                self.peer.choked = false;
                self.events_tx.send(PeerEvent::Unchoke(self.peer.addr)).await?;
            }
            2 => { // Interested
                debug!("Peer {} is interested", self.peer.addr);
                self.peer.interested = true;
                self.events_tx.send(PeerEvent::Interested(self.peer.addr)).await?;
            }
            3 => { // Not Interested
                debug!("Peer {} is not interested", self.peer.addr);
                self.peer.interested = false;
                self.events_tx.send(PeerEvent::NotInterested(self.peer.addr)).await?;
            }
            4 => { // Have
                if data.len() < 5 {
                    return Err(anyhow::anyhow!("Invalid 'have' message"));
                }
                let piece_index = u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as usize;
                trace!("Peer {} has piece {}", self.peer.addr, piece_index);
                self.peer.received_have(piece_index);
                self.events_tx.send(PeerEvent::Have(self.peer.addr, piece_index)).await?;
            }
            5 => { // Bitfield
                if data.len() < 2 {
                    return Err(anyhow::anyhow!("Invalid 'bitfield' message"));
                }
                debug!("Received bitfield from peer {}", self.peer.addr);
                self.peer.update_bitfield(&data[1..], self.total_pieces);
                self.events_tx.send(PeerEvent::Bitfield(self.peer.addr, self.peer.pieces.clone())).await?;
            }
            6 => { // Request
                // We're not handling requests from peers in this implementation
                debug!("Ignoring request from peer {}", self.peer.addr);
            }
            7 => { // Piece
                if data.len() < 9 {
                    return Err(anyhow::anyhow!("Invalid 'piece' message"));
                }

                let piece_index = u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as usize;
                let block_offset = u32::from_be_bytes([data[5], data[6], data[7], data[8]]) as usize;
                let block_data = Bytes::copy_from_slice(&data[9..]);

                trace!("Received block from piece {} at offset {} with size {}",
                       piece_index, block_offset, block_data.len());

                self.events_tx.send(PeerEvent::BlockReceived(
                    self.peer.addr,
                    piece_index,
                    block_offset,
                    block_data,
                )).await?;
            }
            8 => { // Cancel
                // We're not handling cancel messages in this implementation
                debug!("Ignoring cancel from peer {}", self.peer.addr);
            }
            _ => {
                warn!("Unknown message ID: {} from peer {}", message_id, self.peer.addr);
            }
        }

        Ok(())
    }

    /// Send an interested message to the peer
    async fn send_interested(&mut self) -> Result<()> {
        debug!("Sending interested message to peer {}", self.peer.addr);
        let message = Message::new_interested();
        message.write_to(&mut self.stream).await
            .context("Failed to send interested message")?;
        self.peer.am_interested = true;
        Ok(())
    }

    /// Request a block from the peer
    pub async fn request_block(&mut self, piece_index: u32, block_offset: u32, block_length: u32) -> Result<()> {
        if self.peer.choked {
            return Err(anyhow::anyhow!("Cannot request block - we are choked"));
        }

        trace!("Requesting piece {} block at offset {} with length {}",
               piece_index, block_offset, block_length);

        let message = Message::new_request(piece_index, block_offset, block_length);
        message.write_to(&mut self.stream).await
            .context("Failed to send request message")?;

        Ok(())
    }

    /// Send keep-alive message
    pub async fn send_keep_alive(&mut self) -> Result<()> {
        trace!("Sending keep-alive to peer {}", self.peer.addr);
        let message = Message::new_keepalive();
        message.write_to(&mut self.stream).await
            .context("Failed to send keep-alive message")?;
        Ok(())
    }

    /// Run a detached peer connection
    pub async fn run_detached(addr: SocketAddr, events_tx: mpsc::Sender<PeerEvent>) -> Result<()> {
        debug!("Starting detached connection to peer at {}", addr);

        // Create a new TCP connection
        let mut stream = match timeout(
            Duration::from_secs(30), // Default timeout
            TcpStream::connect(addr)
        ).await {
            Ok(stream) => match stream {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to connect to peer at {}: {}", addr, e);
                    events_tx.send(PeerEvent::Disconnected(addr, Some(e.to_string()))).await?;
                    return Err(e.into());
                }
            },
            Err(e) => {
                warn!("Connection timeout for peer at {}: {}", addr, e);
                events_tx.send(PeerEvent::Disconnected(addr, Some("Connection timeout".to_string()))).await?;
                return Err(anyhow::anyhow!("Connection timeout"));
            }
        };

        // Since we don't have the required info_hash, peer_id, and total_pieces,
        // create a minimal viable connection
        let mut peer = Peer::new(addr);

        // Send a notification that we've established a TCP connection
        events_tx.send(PeerEvent::Connected(addr)).await
            .context("Failed to send connection event")?;

        // Keep the connection alive for a while to simulate activity
        let mut buffer = [0u8; 1024];
        loop {
            match stream.read(&mut buffer).await {
                Ok(0) => {
                    // Connection closed
                    debug!("Peer {} closed connection", addr);
                    events_tx.send(PeerEvent::Disconnected(addr, None)).await?;
                    break;
                }
                Ok(n) => {
                    // Process incoming data
                    debug!("Received {} bytes from peer {}", n, addr);
                    // We can't properly process this without info_hash, peer_id, etc.
                }
                Err(e) => {
                    // Error reading from socket
                    warn!("Error reading from peer {}: {}", addr, e);
                    events_tx.send(PeerEvent::Disconnected(addr, Some(e.to_string()))).await?;
                    return Err(e.into());
                }
            }
        }

        Ok(())
    }
}