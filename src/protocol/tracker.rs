//! BitTorrent tracker communication

use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use url::Url;

/// Tracker response for HTTP trackers
#[derive(Debug, Deserialize)]
pub struct HttpTrackerResponse {
    #[serde(default)]
    pub interval: u32,
    #[serde(default)]
    pub complete: u32,
    #[serde(default)]
    pub incomplete: u32,
    #[serde(default)]
    pub peers: Vec<u8>,
    #[serde(rename = "failure reason", default)]
    pub failure_reason: Option<String>,
}

/// Tracker event type for BitTorrent tracker announces
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerEvent {
    /// First announce to tracker when starting a download
    Started,

    /// Announce when stopping a download
    Stopped,

    /// Announce when download is completed
    Completed,

    /// Regular announce with no specific event (empty event string)
    Empty,
}

impl TrackerEvent {
    /// Get the event string for the tracker request
    pub fn value(&self) -> &'static str {
        match self {
            TrackerEvent::Started => "started",
            TrackerEvent::Stopped => "stopped",
            TrackerEvent::Completed => "completed",
            TrackerEvent::Empty => "",
        }
    }
}

impl ToString for TrackerEvent {
    fn to_string(&self) -> String {
        self.value().to_string()
    }
}

/// Events from tracker operations
#[derive(Debug)]
pub enum TrackerOperationEvent {
    /// Successfully connected to tracker
    Connected(String, usize), // tracker url, peer count

    /// Failed to connect to tracker
    Failed(String, String), // tracker url, error reason

    /// Received tracker response
    Response(TrackerResponse),

    /// Tracker reported an error
    Error(String, String), // tracker url, error message

    /// Announce interval has passed, time to re-announce
    IntervalElapsed(String), // tracker url
}

/// Normalized tracker response
#[derive(Debug, Clone)]
pub struct TrackerResponse {
    pub interval: u32,
    pub complete: u32,
    pub incomplete: u32,
    pub peers: Vec<SocketAddr>,
}

/// BitTorrent tracker handler
#[derive(Clone)]
pub struct Tracker {
    pub url: String,
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub client: reqwest::Client,
}

impl Tracker {
    /// Create a new tracker instance
    pub fn new(url: &str, info_hash: [u8; 20], peer_id: [u8; 20], left: u64) -> Self {
        Tracker {
            url: url.to_string(),
            info_hash,
            peer_id,
            port: 6881, // Default BitTorrent port
            uploaded: 0,
            downloaded: 0,
            left,
            client: reqwest::Client::new(),
        }
    }

    /// Announce to the tracker and get peer information
    pub async fn announce(&self, event: TrackerEvent) -> Result<TrackerResponse> {
        info!("Announcing to tracker: {} with event: {}", self.url, event.value());

        if self.url.starts_with("http") {
            self.http_announce(event).await
        } else if self.url.starts_with("udp") {
            Err(anyhow::anyhow!("UDP tracker protocol not implemented"))
        } else {
            Err(anyhow::anyhow!("Unsupported tracker protocol: {}", self.url))
        }
    }

    /// Announce to the tracker and send events through a channel
    pub async fn announce_with_events(
        &self,
        event: TrackerEvent,
        tx: tokio::sync::mpsc::Sender<TrackerOperationEvent>
    ) -> Result<()> {
        let tracker_url = self.url.clone();

        match self.announce(event).await {
            Ok(response) => {
                // Send connected event
                let _ = tx.send(TrackerOperationEvent::Connected(
                    tracker_url.clone(),
                    response.peers.len()
                )).await;

                // Send response event
                let _ = tx.send(TrackerOperationEvent::Response(response)).await;

                Ok(())
            },
            Err(e) => {
                // Send failed event
                let _ = tx.send(TrackerOperationEvent::Failed(
                    tracker_url,
                    e.to_string()
                )).await;

                Err(e)
            }
        }
    }

    /// Announce to an HTTP tracker
    async fn http_announce(&self, event: TrackerEvent) -> Result<TrackerResponse> {
        let mut url = Url::parse(&self.url).context("Failed to parse tracker URL")?;

        // Add query parameters - this shouldn't be held across an await point
        {
            let mut query_pairs = url.query_pairs_mut();

            // Add required parameters
            query_pairs.append_pair("info_hash", &self.url_encode_bytes(&self.info_hash));
            query_pairs.append_pair("peer_id", &self.url_encode_bytes(&self.peer_id));
            query_pairs.append_pair("port", &self.port.to_string());
            query_pairs.append_pair("uploaded", &self.uploaded.to_string());
            query_pairs.append_pair("downloaded", &self.downloaded.to_string());
            query_pairs.append_pair("left", &self.left.to_string());
            query_pairs.append_pair("compact", "1"); // Request compact response

            // Add event if it's not empty
            if event != TrackerEvent::Empty {
                query_pairs.append_pair("event", &event.to_string());
            }
        } // query_pairs is dropped here, freeing the borrow on url

        debug!("HTTP tracker request URL: {}", url);

        // Send the request
        let response = self.client
            .get(url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context("Failed to send HTTP request to tracker")?;

        // Check for HTTP errors
        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Tracker returned HTTP error: {}",
                response.status()
            ));
        }

        // Get response bytes
        let bytes = response.bytes().await
            .context("Failed to read tracker response")?;

        // Decode the bencode response
        let tracker_response: HttpTrackerResponse = serde_bencode::from_bytes(&bytes)
            .context("Failed to decode bencoded response")?;

        // Check for tracker error
        if let Some(error) = tracker_response.failure_reason {
            return Err(anyhow::anyhow!("Tracker error: {}", error));
        }

        // Parse peers from compact format
        let mut peers = Vec::new();

        // Compact format is 6 bytes per peer (4 for IP, 2 for port)
        for chunk in tracker_response.peers.chunks(6) {
            if chunk.len() == 6 {
                let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
                let port = ((chunk[4] as u16) << 8) | (chunk[5] as u16);
                peers.push(SocketAddr::new(IpAddr::V4(ip), port));
            }
        }

        info!("Received {} peers from tracker", peers.len());

        Ok(TrackerResponse {
            interval: tracker_response.interval,
            complete: tracker_response.complete,
            incomplete: tracker_response.incomplete,
            peers,
        })
    }

    /// URL encode bytes for query parameters
    fn url_encode_bytes(&self, bytes: &[u8]) -> String {
        let mut result = String::with_capacity(bytes.len() * 3);
        for &byte in bytes {
            if (byte as char).is_alphanumeric() {
                result.push(byte as char);
            } else {
                result.push('%');
                result.push_str(&format!("{:02X}", byte));
            }
        }
        result
    }

    /// Update stats for the next announce
    pub fn update_stats(&mut self, uploaded: u64, downloaded: u64, left: u64) {
        self.uploaded = uploaded;
        self.downloaded = downloaded;
        self.left = left;
    }
}