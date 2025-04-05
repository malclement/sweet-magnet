use crate::error::SweetMagnetError;
use anyhow::Result;
use std::collections::HashMap;

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

        // Get the query part of the URI (everything after "magnet:?")
        let query = &magnet_uri[8..];

        // Split the query into key-value pairs
        let mut params = HashMap::new();
        let mut trackers = Vec::new();
        let mut info_hash = None;
        let mut display_name = None;

        for pair in query.split('&') {
            if let Some(index) = pair.find('=') {
                let (key, value) = pair.split_at(index);
                let value = &value[1..]; // Skip the '=' character

                // URL decode the value
                let decoded_value = percent_decode(value);

                match key {
                    "xt" => {
                        if decoded_value.starts_with("urn:btih:") {
                            let hash = decoded_value.trim_start_matches("urn:btih:");
                            if hash.len() != 40 {
                                return Err(SweetMagnetError::MagnetParse(
                                    "Invalid info hash length".to_string(),
                                ));
                            }
                            info_hash = Some(hash.to_string());
                        }
                    },
                    "dn" => {
                        display_name = Some(decoded_value.clone());
                    },
                    "tr" => {
                        trackers.push(decoded_value.clone());
                    },
                    _ => {
                        if key.starts_with("tr.") || key.starts_with("tr") {
                            trackers.push(decoded_value.clone());
                        }
                    }
                }

                params.insert(key.to_string(), decoded_value);
            }
        }

        // Validate that we have the required info hash
        let info_hash = match info_hash {
            Some(hash) => hash,
            None => return Err(SweetMagnetError::MagnetParse(
                "Missing or invalid info hash".to_string(),
            )),
        };

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

/// Helper function to decode URL-encoded strings
fn percent_decode(input: &str) -> String {
    // First replace '+' with spaces
    let input = input.replace('+', " ");

    // Now handle percent encoding
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
            // Try to get the next two characters for percent decoding
            let hex_char1 = chars.next();
            let hex_char2 = chars.next();

            match (hex_char1, hex_char2) {
                (Some(h1), Some(h2)) => {
                    if let Ok(byte) = u8::from_str_radix(&format!("{}{}", h1, h2), 16) {
                        output.push(byte as char);
                    } else {
                        // If decoding fails, just keep the original sequence
                        output.push('%');
                        output.push(h1);
                        output.push(h2);
                    }
                },
                _ => {
                    // If we don't have two characters after %, keep the % and whatever else we have
                    output.push('%');
                    if let Some(h1) = hex_char1 {
                        output.push(h1);
                    }
                }
            }
        } else {
            output.push(c);
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_magnet() {
        let magnet = "magnet:?xt=urn:btih:e45deff382bcfb0342b00b638414eb063de304f9&dn=Test+Torrent&tr=udp%3A%2F%2Ftracker.example.org%3A1337%2Fannounce";
        let parsed = MagnetLink::parse(magnet).unwrap();

        // Print debug info for test
        println!("Input magnet: {}", magnet);
        println!("Parsed display name: {:?}", parsed.display_name);

        assert_eq!(parsed.info_hash, "e45deff382bcfb0342b00b638414eb063de304f9");
        assert_eq!(parsed.display_name, Some("Test Torrent".to_string()));
        assert_eq!(parsed.trackers.len(), 1);
    }

    #[test]
    fn test_parse_invalid_magnet() {
        let result = MagnetLink::parse("https://example.com");
        assert!(result.is_err());
    }

    #[test]
    fn test_percent_decode() {
        assert_eq!(percent_decode("hello+world"), "hello world");
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("test%3A%2F%2Fexample"), "test://example");
    }
}