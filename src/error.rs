use std::fmt;
use std::io;

#[derive(Debug)]
pub enum SweetMagnetError {
    Io(io::Error),
    MagnetParse(String),
    TorrentClient(String),
    MediaPlayer(String),
}

impl fmt::Display for SweetMagnetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SweetMagnetError::Io(e) => write!(f, "I/O error: {}", e),
            SweetMagnetError::MagnetParse(s) => write!(f, "Magnet parsing error: {}", s),
            SweetMagnetError::TorrentClient(s) => write!(f, "Torrent client error: {}", s),
            SweetMagnetError::MediaPlayer(s) => write!(f, "Media player error: {}", s),
        }
    }
}

impl std::error::Error for SweetMagnetError {}

impl From<io::Error> for SweetMagnetError {
    fn from(error: io::Error) -> Self {
        SweetMagnetError::Io(error)
    }
}