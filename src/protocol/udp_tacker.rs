use anyhow::{Context, Result};
use bytes::{Buf, BufMut, BytesMut};
use log::{debug, info, warn};
use rand::Rng;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::timeout;
use url::Url;

/// Constants for UDP tracker protocol
const UDP_PROTOCOL_ID: u64 = 0x41727101980;
const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_SCRAPE: u32 = 2;
const ACTION_ERROR: u32 = 3;
const UDP_TRANSACTION_TIMEOUT: u64 = 15; // 15 seconds

/// UDP tracker implementation
pub struct UdpTracker {
    url: String,
    socket: UdpSocket,
    connection_id: Option<u64>,
    transaction_id: u32,
    timeout: Duration,
}

impl UdpTracker {
    /// Create a new UDP tracker instance
    pub async fn new(url: &str) -> Result<Self> {
        // Parse UDP tracker URL
        let parsed_url = Url::parse(url).context("Failed to parse UDP tracker URL")?;

        if parsed_url.scheme() != "udp" {
            return Err(anyhow::anyhow!("Not a UDP tracker URL"));
        }

        let host = parsed_url.host_str().ok_or_else(|| anyhow::anyhow!("Missing host in tracker URL"))?;
        let port = parsed_url.port().unwrap_or(80);

        // Create UDP socket
        let socket = UdpSocket::bind("0.0.0.0:0").await.context("Failed to bind UDP socket")?;

        // Generate random transaction ID
        let mut rng = rand::thread_rng();
        let transaction_id = rng.random::<u32>();

        Ok(UdpTracker {
            url: url.to_string(),
            socket,
            connection_id: None,
            transaction_id,
            timeout: Duration::from_secs(UDP_TRANSACTION_TIMEOUT),
        })
    }

    /// Connect to the UDP tracker to get a connection ID
    async fn connect(&mut self) -> Result<()> {
        // Parse tracker URL again to get host and port
        let parsed_url = Url::parse(&self.url).context("Failed to parse UDP tracker URL")?;
        let host = parsed_url.host_str().ok_or_else(|| anyhow::anyhow!("Missing host in tracker URL"))?;
        let port = parsed_url.port().unwrap_or(80);

        // Resolve hostname to IP
        let addr = format!("{}:{}", host, port);
        let resolved = tokio::net::lookup_host(&addr).await.context("Failed to resolve hostname")?;
        let addr = resolved.into_iter().next().ok_or_else(|| anyhow::anyhow!("No addresses found for hostname"))?;

        // Connect message format:
        // Offset  Size    Name    Value
        // 0       64-bit  protocol_id     0x41727101980
        // 8       32-bit  action          0 // connect
        // 12      32-bit  transaction_id  random
        let mut buffer = BytesMut::with_capacity(16);
        buffer.put_u64(UDP_PROTOCOL_ID);
        buffer.put_u32(ACTION_CONNECT);
        buffer.put_u32(self.transaction_id);

        // Send the connection request
        self.socket.send_to(&buffer, addr).await.context("Failed to send connection request")?;

        // Wait for response
        let mut response = [0u8; 16];
        let result = timeout(self.timeout, self.socket.recv_from(&mut response)).await;

        match result {
            Ok(Ok((len, _))) => {
                if len < 16 {
                    return Err(anyhow::anyhow!("Received truncated response"));
                }

                // Parse response
                let mut cursor = std::io::Cursor::new(&response[..16]);
                let action = cursor.get_u32();
                let returned_transaction_id = cursor.get_u32();

                // Validate response
                if action != ACTION_CONNECT {
                    return Err(anyhow::anyhow!("Expected connect action, got {}", action));
                }

                if returned_transaction_id != self.transaction_id {
                    return Err(anyhow::anyhow!("Transaction ID mismatch"));
                }

                // Get the connection ID
                let connection_id = cursor.get_u64();
                self.connection_id = Some(connection_id);

                debug!("Connected to UDP tracker: {}", self.url);
                Ok(())
            },
            Ok(Err(e)) => Err(anyhow::anyhow!("Socket error: {}", e)),
            Err(_) => Err(anyhow::anyhow!("Connection timeout")),
        }
    }

    /// Announce to the UDP tracker
    pub async fn announce(
        &mut self,
        info_hash: [u8; 20],
        peer_id: [u8; 20],
        downloaded: u64,
        left: u64,
        uploaded: u64,
        event: u32,  // 0: none, 1: completed, 2: started, 3: stopped
        port: u16,
    ) -> Result<Vec<SocketAddr>> {
        // Connect if we don't have a connection ID
        if self.connection_id.is_none() {
            self.connect().await.context("Failed to connect to UDP tracker")?;
        }

        // Parse tracker URL to get host and port
        let parsed_url = Url::parse(&self.url).context("Failed to parse UDP tracker URL")?;
        let host = parsed_url.host_str().ok_or_else(|| anyhow::anyhow!("Missing host in tracker URL"))?;
        let port = parsed_url.port().unwrap_or(80);

        // Resolve hostname to IP
        let addr = format!("{}:{}", host, port);
        let resolved = tokio::net::lookup_host(&addr).await.context("Failed to resolve hostname")?;
        let addr = resolved.into_iter().next().ok_or_else(|| anyhow::anyhow!("No addresses found for hostname"))?;

        // UDP announce request:
        // Offset  Size    Name    Value
        // 0       64-bit  connection_id (from connect)
        // 8       32-bit  action          1 // announce
        // 12      32-bit  transaction_id  random
        // 16      20-byte info_hash
        // 36      20-byte peer_id
        // 56      64-bit  downloaded
        // 64      64-bit  left
        // 72      64-bit  uploaded
        // 80      32-bit  event           0: none; 1: completed; 2: started; 3: stopped
        // 84      32-bit  IP address      0 // default
        // 88      32-bit  key             random
        // 92      32-bit  num_want        -1 // default
        // 96      16-bit  port            Listen port

        let connection_id = self.connection_id.unwrap();
        let mut buffer = BytesMut::with_capacity(98);

        buffer.put_u64(connection_id);
        buffer.put_u32(ACTION_ANNOUNCE);
        buffer.put_u32(self.transaction_id);
        buffer.put_slice(&info_hash);
        buffer.put_slice(&peer_id);
        buffer.put_u64(downloaded);
        buffer.put_u64(left);
        buffer.put_u64(uploaded);
        buffer.put_u32(event);
        buffer.put_u32(0); // IP address (default)
        buffer.put_u32(rand::random()); // Random key
        buffer.put_i32(-1); // num_want (default)
        buffer.put_u16(port);

        // Send the announce request
        self.socket.send_to(&buffer, addr).await.context("Failed to send announce request")?;

        // Wait for response
        let mut response = [0u8; 2048]; // Big enough for many peers
        let result = timeout(self.timeout, self.socket.recv_from(&mut response)).await;

        match result {
            Ok(Ok((len, _))) => {
                if len < 20 {
                    return Err(anyhow::anyhow!("Received truncated response"));
                }

                // Parse response
                let mut cursor = std::io::Cursor::new(&response[..len]);
                let action = cursor.get_u32();
                let returned_transaction_id = cursor.get_u32();

                // Validate response
                if action != ACTION_ANNOUNCE {
                    if action == ACTION_ERROR {
                        let error_msg = String::from_utf8_lossy(&response[8..len]);
                        return Err(anyhow::anyhow!("Tracker error: {}", error_msg));
                    }
                    return Err(anyhow::anyhow!("Expected announce action, got {}", action));
                }

                if returned_transaction_id != self.transaction_id {
                    return Err(anyhow::anyhow!("Transaction ID mismatch"));
                }

                // Get announce data
                let interval = cursor.get_u32();
                let leechers = cursor.get_u32();
                let seeders = cursor.get_u32();

                // Parse peers
                let mut peers = Vec::new();

                // Each peer is 6 bytes: 4 for IP, 2 for port
                while cursor.position() + 6 <= len as u64 {
                    let ip = Ipv4Addr::new(
                        cursor.get_u8(),
                        cursor.get_u8(),
                        cursor.get_u8(),
                        cursor.get_u8(),
                    );
                    let port = cursor.get_u16();

                    peers.push(SocketAddr::new(IpAddr::V4(ip), port));
                }

                info!("Got {} peers from UDP tracker {} (seeders: {}, leechers: {})",
                     peers.len(), self.url, seeders, leechers);

                Ok(peers)
            },
            Ok(Err(e)) => Err(anyhow::anyhow!("Socket error: {}", e)),
            Err(_) => Err(anyhow::anyhow!("Announce timeout")),
        }
    }
}