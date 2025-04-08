use anyhow::{Context, Result};
use bytes::{Buf, BufMut, BytesMut};
use log::{debug, info, warn, error};
use rand::Rng;
use serde_bencode::{self, value::Value};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::timeout;

/// DHT message types
#[derive(Debug, PartialEq, Eq)]
enum DhtMessageType {
    Query,
    Response,
    Error,
}

/// DHT node information
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DhtNode {
    id: [u8; 20],
    addr: SocketAddr,
}

/// DHT implementation for BitTorrent
pub struct Dht {
    socket: UdpSocket,
    node_id: [u8; 20],
    bootstrap_nodes: Vec<SocketAddr>,
    known_nodes: HashSet<DhtNode>,
    transaction_id: u32,
}

impl Dht {
    /// Create a new DHT instance
    pub async fn new() -> Result<Self> {
        // Generate a random node ID
        let mut node_id = [0u8; 20];
        {
            // Use correct function name and scope the RNG so it's dropped before await
            let mut rng = rand::rng();
            rng.fill(&mut node_id);
        } // RNG is dropped here, before any await points

        // Create a UDP socket
        let socket = UdpSocket::bind("0.0.0.0:0").await.context("Failed to bind DHT socket")?;

        // Default bootstrap nodes (well-known DHT nodes)
        let bootstrap_nodes = vec![
            "router.bittorrent.com:6881",
            "dht.transmissionbt.com:6881",
            "router.utorrent.com:6881",
        ];

        // Resolve bootstrap nodes
        let mut resolved_nodes = Vec::new();
        for node in bootstrap_nodes {
            match tokio::net::lookup_host(node).await {
                Ok(addrs) => {
                    for addr in addrs {
                        resolved_nodes.push(addr);
                    }
                },
                Err(e) => {
                    warn!("Failed to resolve bootstrap node {}: {}", node, e);
                }
            }
        }

        info!("DHT initialized with {} bootstrap nodes", resolved_nodes.len());

        Ok(Dht {
            socket,
            node_id,
            bootstrap_nodes: resolved_nodes,
            known_nodes: HashSet::new(),
            transaction_id: 0,
        })
    }

    /// Start the DHT by contacting bootstrap nodes
    pub async fn bootstrap(&mut self) -> Result<()> {
        info!("Starting DHT bootstrap");

        // Copy the bootstrap nodes to avoid borrowing conflict
        let bootstrap_addrs: Vec<SocketAddr> = self.bootstrap_nodes.clone();
        for &addr in &bootstrap_addrs {
            let _ = self.ping(addr).await;
        }

        // Wait a bit to collect nodes
        tokio::time::sleep(Duration::from_secs(2)).await;

        info!("DHT bootstrap complete, known nodes: {}", self.known_nodes.len());
        Ok(())
    }

    /// Send a ping message to a node
    async fn ping(&mut self, addr: SocketAddr) -> Result<()> {
        // Increment transaction ID
        self.transaction_id = self.transaction_id.wrapping_add(1);
        let transaction_id = self.transaction_id.to_string();

        // Build ping message with HashMap
        let mut ping_msg_dict = HashMap::new();
        ping_msg_dict.insert(b"t".to_vec(), Value::Bytes(transaction_id.into_bytes()));
        ping_msg_dict.insert(b"y".to_vec(), Value::Bytes(b"q".to_vec()));
        ping_msg_dict.insert(b"q".to_vec(), Value::Bytes(b"ping".to_vec()));

        // Create the "a" dictionary
        let mut a_dict = HashMap::new();
        a_dict.insert(b"id".to_vec(), Value::Bytes(self.node_id.to_vec()));
        ping_msg_dict.insert(b"a".to_vec(), Value::Dict(a_dict));

        let ping_msg = serde_bencode::value::Value::Dict(ping_msg_dict);

        // Encode the message
        let encoded = serde_bencode::to_bytes(&ping_msg)?;

        // Send the ping
        self.socket.send_to(&encoded, addr).await?;

        // We're not waiting for response here, just fire and forget
        debug!("Sent DHT ping to {}", addr);
        Ok(())
    }

    /// Find nodes near a target ID
    async fn find_node(&mut self, addr: SocketAddr, target: &[u8; 20]) -> Result<()> {
        // Increment transaction ID
        self.transaction_id = self.transaction_id.wrapping_add(1);
        let transaction_id = self.transaction_id.to_string();

        // Build find_node message with HashMap
        let mut find_node_msg_dict = HashMap::new();
        find_node_msg_dict.insert(b"t".to_vec(), Value::Bytes(transaction_id.into_bytes()));
        find_node_msg_dict.insert(b"y".to_vec(), Value::Bytes(b"q".to_vec()));
        find_node_msg_dict.insert(b"q".to_vec(), Value::Bytes(b"find_node".to_vec()));

        // Create the "a" dictionary
        let mut a_dict = HashMap::new();
        a_dict.insert(b"id".to_vec(), Value::Bytes(self.node_id.to_vec()));
        a_dict.insert(b"target".to_vec(), Value::Bytes(target.to_vec()));
        find_node_msg_dict.insert(b"a".to_vec(), Value::Dict(a_dict));

        let find_node_msg = serde_bencode::value::Value::Dict(find_node_msg_dict);

        // Encode the message
        let encoded = serde_bencode::to_bytes(&find_node_msg)?;

        // Send the find_node request
        self.socket.send_to(&encoded, addr).await?;

        // We're not waiting for response here, just fire and forget
        debug!("Sent DHT find_node to {}", addr);
        Ok(())
    }

    /// Get peers for an info hash
    async fn get_peers(&mut self, addr: SocketAddr, info_hash: &[u8; 20]) -> Result<()> {
        // Increment transaction ID
        self.transaction_id = self.transaction_id.wrapping_add(1);
        let transaction_id = self.transaction_id.to_string();

        // Build get_peers message with HashMap
        let mut get_peers_msg_dict = HashMap::new();
        get_peers_msg_dict.insert(b"t".to_vec(), Value::Bytes(transaction_id.into_bytes()));
        get_peers_msg_dict.insert(b"y".to_vec(), Value::Bytes(b"q".to_vec()));
        get_peers_msg_dict.insert(b"q".to_vec(), Value::Bytes(b"get_peers".to_vec()));

        // Create the "a" dictionary
        let mut a_dict = HashMap::new();
        a_dict.insert(b"id".to_vec(), Value::Bytes(self.node_id.to_vec()));
        a_dict.insert(b"info_hash".to_vec(), Value::Bytes(info_hash.to_vec()));
        get_peers_msg_dict.insert(b"a".to_vec(), Value::Dict(a_dict));

        let get_peers_msg = serde_bencode::value::Value::Dict(get_peers_msg_dict);

        // Encode the message
        let encoded = serde_bencode::to_bytes(&get_peers_msg)?;

        // Send the get_peers request
        self.socket.send_to(&encoded, addr).await?;

        debug!("Sent DHT get_peers to {}", addr);
        Ok(())
    }

    /// Process incoming DHT messages
    async fn process_message(&mut self, data: &[u8], from: SocketAddr) -> Result<Vec<SocketAddr>> {
        let mut peers = Vec::new();

        // Try to decode the bencode message
        let msg = match serde_bencode::from_bytes::<Value>(data) {
            Ok(v) => v,
            Err(e) => {
                debug!("Failed to parse DHT message: {}", e);
                return Ok(peers);
            }
        };

        // Extract message type
        let msg_type = if let Value::Dict(dict) = &msg {
            if let Some(Value::Bytes(y)) = dict.iter().find(|(k, _)| k.as_slice() == b"y").map(|(_, v)| v) {
                match y.as_slice() {
                    b"q" => DhtMessageType::Query,
                    b"r" => DhtMessageType::Response,
                    b"e" => DhtMessageType::Error,
                    _ => {
                        debug!("Unknown DHT message type");
                        return Ok(peers);
                    }
                }
            } else {
                debug!("Missing message type in DHT message");
                return Ok(peers);
            }
        } else {
            debug!("DHT message is not a dictionary");
            return Ok(peers);
        };

        // Process based on message type
        match msg_type {
            DhtMessageType::Response => {
                // Extract response data
                if let Value::Dict(dict) = &msg {
                    if let Some(Value::Dict(r)) = dict.iter().find(|(k, _)| k.as_slice() == b"r").map(|(_, v)| v) {
                        // Process nodes
                        if let Some(Value::Bytes(nodes)) = r.iter().find(|(k, _)| k.as_slice() == b"nodes").map(|(_, v)| v) {
                            self.process_nodes(nodes.as_slice());
                        }

                        // Process peers
                        if let Some(Value::List(values)) = r.iter().find(|(k, _)| k.as_slice() == b"values").map(|(_, v)| v) {
                            for value in values {
                                if let Value::Bytes(peer_bytes) = value {
                                    if peer_bytes.len() == 6 {
                                        let ip = Ipv4Addr::new(
                                            peer_bytes[0], peer_bytes[1], peer_bytes[2], peer_bytes[3]
                                        );
                                        let port = ((peer_bytes[4] as u16) << 8) | (peer_bytes[5] as u16);
                                        peers.push(SocketAddr::new(IpAddr::V4(ip), port));
                                    }
                                }
                            }
                        }
                    }
                }
            },
            DhtMessageType::Query => {
                // We would respond to queries here, but for now we're just ignoring them
                debug!("Ignoring DHT query from {}", from);
            },
            DhtMessageType::Error => {
                // Log error messages
                if let Value::Dict(dict) = &msg {
                    if let Some(Value::List(e)) = dict.iter().find(|(k, _)| k.as_slice() == b"e").map(|(_, v)| v) {
                        if e.len() >= 2 {
                            if let (Value::Int(code), Value::Bytes(msg)) = (&e[0], &e[1]) {
                                warn!("DHT error from {}: {} - {}", from, code, String::from_utf8_lossy(msg));
                            }
                        }
                    }
                }
            },
        }

        Ok(peers)
    }

    /// Process nodes information from DHT responses
    fn process_nodes(&mut self, nodes_data: &[u8]) {
        // Nodes are encoded as a string of (20 byte ID + 6 byte address) pairs
        for chunk in nodes_data.chunks(26) {
            if chunk.len() == 26 {
                // Extract node ID (first 20 bytes)
                let mut id = [0u8; 20];
                id.copy_from_slice(&chunk[0..20]);

                // Extract IP and port (last 6 bytes)
                let ip = Ipv4Addr::new(chunk[20], chunk[21], chunk[22], chunk[23]);
                let port = ((chunk[24] as u16) << 8) | (chunk[25] as u16);
                let addr = SocketAddr::new(IpAddr::V4(ip), port);

                // Add to known nodes if it's not ourselves
                if id != self.node_id {
                    let node = DhtNode { id, addr };
                    self.known_nodes.insert(node);
                }
            }
        }
    }

    /// Find peers for a specific info hash
    pub async fn find_peers(&mut self, info_hash: [u8; 20], max_nodes: usize) -> Result<Vec<SocketAddr>> {
        info!("Searching for peers with info hash: {:?}", info_hash);

        // Make sure we have bootstrap nodes
        if self.known_nodes.is_empty() {
            self.bootstrap().await?;
        }

        // If still no nodes, give up
        if self.known_nodes.is_empty() {
            return Err(anyhow::anyhow!("No DHT nodes available"));
        }

        // Send get_peers to known nodes
        let known_nodes: Vec<_> = self.known_nodes.iter().cloned().collect();
        for node in known_nodes.iter().take(max_nodes) {
            let _ = self.get_peers(node.addr, &info_hash).await;

            // Also find_node with the info_hash as target to find closer nodes
            let _ = self.find_node(node.addr, &info_hash).await;
        }

        // Listen for responses for a while
        let mut peers = HashSet::new();
        let start_time = std::time::Instant::now();
        let timeout_duration = Duration::from_secs(8); // Adjust timeout as needed

        while start_time.elapsed() < timeout_duration {
            let mut buf = [0u8; 2048];
            match timeout(Duration::from_secs(1), self.socket.recv_from(&mut buf)).await {
                Ok(Ok((len, addr))) => {
                    if let Ok(new_peers) = self.process_message(&buf[..len], addr).await {
                        for peer in new_peers {
                            peers.insert(peer);
                        }
                    }
                },
                Ok(Err(e)) => {
                    debug!("DHT socket error: {}", e);
                },
                Err(_) => {
                    // Timeout, continue
                }
            }

            // If we've found enough peers, break early
            if peers.len() >= 100 {
                break;
            }
        }

        info!("DHT found {} peers for info hash", peers.len());
        Ok(peers.into_iter().collect())
    }
}