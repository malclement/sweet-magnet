use crate::error::SweetMagnetError;
use anyhow::Result;
use std::collections::HashMap;
use url::Url;

/// Represents a parsed magnet link with its components
#[derive(Debug, Clone)]
pub struct MagnetLink {
    /// Info hash of the torrent
    pub info_hash: String,
    /// Display name of the content
    pub display_name: Option<String>,
    /// List of tracker URLs
    pub trackers: Vec<String>,
    /// Other parameters in the magnet link
    pub params: HashMap<String, String>,
}

impl MagnetLink {
    /// Parse a magnet link from a string
    pub fn parse(magnet_uri: &str) -> Result<Self, SweetMagnetError> {
        if !magnet_uri.starts_with("magnet:?") {
            return Err(SweetMagnetError::MagnetParse(
                "Invalid magnet link format".to_string(),
            ));
        }

        // Parse the magnet URI using the url crate
        let parsed_url = match Url::parse(magnet_uri) {
            Ok(url) => url,
            Err(_) => {
                return Err(SweetMagnetError::MagnetParse(
                    "Failed to parse magnet URL".to_string(),
                ))
            }
        };

        // Extract query parameters
        let params: HashMap<String, String> = parsed_url
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        // Extract the info hash
        let info_hash = match params.get("xt") {
            Some(xt) if xt.starts_with("urn:btih:") => {
                let hash = xt.trim_start_matches("urn:btih:");
                if hash.len() != 40 {
                    return Err(SweetMagnetError::MagnetParse(
                        "Invalid info hash length".to_string(),
                    ));
                }
                hash.to_string()
            }
            _ => {
                return Err(SweetMagnetError::MagnetParse(
                    "Missing or invalid info hash".to_string(),
                ))
            }
        };

        // Extract display name
        let display_name = params.get("dn").cloned();

        // Extract trackers
        let mut trackers = Vec::new();
        for (key, value) in &params {
            if key.starts_with("tr") {
                trackers.push(value.clone());
            }
        }

        Ok(MagnetLink {
            info_hash,
            display_name,
            trackers,
            params,
        })
    }

    /// Get the torrent info hash
    pub fn info_hash(&self) -> &str {
        &self.info_hash
    }

    /// Get the display name or a default based on the info hash
    pub fn name(&self) -> String {
        self.display_name
            .clone()
            .unwrap_or_else(|| format!("torrent-{}", self.info_hash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_magnet() {
        let magnet = "magnet:?xt=urn:btih:e45deff382bcfb0342b00b638414eb063de304f9&dn=Test+Torrent&tr=udp%3A%2F%2Ftracker.example.org%3A1337%2Fannounce";
        let parsed = MagnetLink::parse(magnet).unwrap();

        assert_eq!(parsed.info_hash, "e45deff382bcfb0342b00b638414eb063de304f9");
        assert_eq!(parsed.display_name, Some("Test+Torrent".to_string()));
        assert_eq!(parsed.trackers.len(), 1);
    }

    #[test]
    fn test_parse_invalid_magnet() {
        let result = MagnetLink::parse("https://example.com");
        assert!(result.is_err());
    }
}