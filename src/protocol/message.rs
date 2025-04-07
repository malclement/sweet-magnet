//! BitTorrent protocol message handling

use std::io::{self, Cursor};
use std::mem;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// BitTorrent message types
#[derive(Debug, Clone, PartialEq)]
pub enum MessageType {
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have,
    Bitfield,
    Request,
    Piece,
    Cancel,
    Port,
    Handshake,
    KeepAlive,
}

/// BitTorrent protocol message
#[derive(Debug, Clone)]
pub struct Message {
    pub message_type: MessageType,
    pub payload: Bytes,
}

impl Message {
    // Constants for message IDs
    const CHOKE: u8 = 0;
    const UNCHOKE: u8 = 1;
    const INTERESTED: u8 = 2;
    const NOT_INTERESTED: u8 = 3;
    const HAVE: u8 = 4;
    const BITFIELD: u8 = 5;
    const REQUEST: u8 = 6;
    const PIECE: u8 = 7;
    const CANCEL: u8 = 8;
    const PORT: u8 = 9;

    // Message size constants
    const HAVE_LENGTH: u32 = 5;
    const REQUEST_LENGTH: u32 = 13;
    const CANCEL_LENGTH: u32 = 13;

    /// Create a new handshake message
    pub fn new_handshake(info_hash: &[u8; 20], peer_id: &[u8; 20]) -> Self {
        let mut payload = BytesMut::with_capacity(68);

        // Protocol string length (1 byte)
        payload.put_u8(19);

        // Protocol string (19 bytes) - "BitTorrent protocol"
        payload.put_slice(b"BitTorrent protocol");

        // Reserved bytes (8 bytes)
        payload.put_bytes(0, 8);

        // Info hash (20 bytes)
        payload.put_slice(info_hash);

        // Peer ID (20 bytes)
        payload.put_slice(peer_id);

        Message {
            message_type: MessageType::Handshake,
            payload: payload.freeze(),
        }
    }

    /// Create a new keepalive message
    pub fn new_keepalive() -> Self {
        Message {
            message_type: MessageType::KeepAlive,
            payload: Bytes::new(),
        }
    }

    /// Create a new choke message
    pub fn new_choke() -> Self {
        let mut payload = BytesMut::with_capacity(5);
        payload.put_u32(1); // Length
        payload.put_u8(Self::CHOKE); // ID

        Message {
            message_type: MessageType::Choke,
            payload: payload.freeze(),
        }
    }

    /// Create a new unchoke message
    pub fn new_unchoke() -> Self {
        let mut payload = BytesMut::with_capacity(5);
        payload.put_u32(1); // Length
        payload.put_u8(Self::UNCHOKE); // ID

        Message {
            message_type: MessageType::Unchoke,
            payload: payload.freeze(),
        }
    }

    /// Create a new interested message
    pub fn new_interested() -> Self {
        let mut payload = BytesMut::with_capacity(5);
        payload.put_u32(1); // Length
        payload.put_u8(Self::INTERESTED); // ID

        Message {
            message_type: MessageType::Interested,
            payload: payload.freeze(),
        }
    }

    /// Create a new not interested message
    pub fn new_not_interested() -> Self {
        let mut payload = BytesMut::with_capacity(5);
        payload.put_u32(1); // Length
        payload.put_u8(Self::NOT_INTERESTED); // ID

        Message {
            message_type: MessageType::NotInterested,
            payload: payload.freeze(),
        }
    }

    /// Create a new have message
    pub fn new_have(piece_index: u32) -> Self {
        let mut payload = BytesMut::with_capacity(9);
        payload.put_u32(5); // Length
        payload.put_u8(Self::HAVE); // ID
        payload.put_u32(piece_index); // Piece Index

        Message {
            message_type: MessageType::Have,
            payload: payload.freeze(),
        }
    }

    /// Create a new bitfield message
    pub fn new_bitfield(bitfield: &[u8]) -> Self {
        let length = 1 + bitfield.len() as u32;
        let mut payload = BytesMut::with_capacity(4 + length as usize);
        payload.put_u32(length); // Length
        payload.put_u8(Self::BITFIELD); // ID
        payload.put_slice(bitfield); // Bitfield

        Message {
            message_type: MessageType::Bitfield,
            payload: payload.freeze(),
        }
    }

    /// Create a new request message
    pub fn new_request(index: u32, begin: u32, length: u32) -> Self {
        let mut payload = BytesMut::with_capacity(17);
        payload.put_u32(13); // Length
        payload.put_u8(Self::REQUEST); // ID
        payload.put_u32(index); // Piece Index
        payload.put_u32(begin); // Begin Offset
        payload.put_u32(length); // Request Length

        Message {
            message_type: MessageType::Request,
            payload: payload.freeze(),
        }
    }

    /// Create a new piece message
    pub fn new_piece(index: u32, begin: u32, block: &[u8]) -> Self {
        let length = 9 + block.len() as u32;
        let mut payload = BytesMut::with_capacity(4 + length as usize);
        payload.put_u32(length); // Length
        payload.put_u8(Self::PIECE); // ID
        payload.put_u32(index); // Piece Index
        payload.put_u32(begin); // Begin Offset
        payload.put_slice(block); // Block Data

        Message {
            message_type: MessageType::Piece,
            payload: payload.freeze(),
        }
    }

    /// Create a new cancel message
    pub fn new_cancel(index: u32, begin: u32, length: u32) -> Self {
        let mut payload = BytesMut::with_capacity(17);
        payload.put_u32(13); // Length
        payload.put_u8(Self::CANCEL); // ID
        payload.put_u32(index); // Piece Index
        payload.put_u32(begin); // Begin Offset
        payload.put_u32(length); // Request Length

        Message {
            message_type: MessageType::Cancel,
            payload: payload.freeze(),
        }
    }

    /// Parse a message from bytes
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Message too short"));
        }

        let mut cursor = Cursor::new(bytes);

        // Check if it's a handshake
        if bytes.len() >= 68 && bytes[0] == 19 && &bytes[1..20] == b"BitTorrent protocol" {
            return Ok(Message {
                message_type: MessageType::Handshake,
                payload: Bytes::copy_from_slice(bytes),
            });
        }

        // Regular message handling
        let length = cursor.get_u32();

        // KeepAlive message (length = 0)
        if length == 0 {
            return Ok(Message {
                message_type: MessageType::KeepAlive,
                payload: Bytes::new(),
            });
        }

        // Ensure we have enough data
        if bytes.len() < 4 + length as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Incomplete message"));
        }

        let id = cursor.get_u8();
        let message_type = match id {
            Self::CHOKE => MessageType::Choke,
            Self::UNCHOKE => MessageType::Unchoke,
            Self::INTERESTED => MessageType::Interested,
            Self::NOT_INTERESTED => MessageType::NotInterested,
            Self::HAVE => MessageType::Have,
            Self::BITFIELD => MessageType::Bitfield,
            Self::REQUEST => MessageType::Request,
            Self::PIECE => MessageType::Piece,
            Self::CANCEL => MessageType::Cancel,
            Self::PORT => MessageType::Port,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Unknown message ID: {}", id))),
        };

        Ok(Message {
            message_type,
            payload: Bytes::copy_from_slice(bytes),
        })
    }

    /// Extract piece index from a 'have' message
    pub fn get_have_piece_index(&self) -> Option<u32> {
        if let MessageType::Have = self.message_type {
            if self.payload.len() >= 9 { // 4 (length) + 1 (id) + 4 (piece index)
                let mut cursor = Cursor::new(&self.payload[5..9]);
                return Some(cursor.get_u32());
            }
        }
        None
    }

    /// Extract bitfield from a 'bitfield' message
    pub fn get_bitfield(&self) -> Option<Bytes> {
        if let MessageType::Bitfield = self.message_type {
            if self.payload.len() > 5 { // 4 (length) + 1 (id) + bitfield
                return Some(Bytes::copy_from_slice(&self.payload[5..]));
            }
        }
        None
    }

    /// Extract piece data from a 'piece' message
    pub fn get_piece_data(&self) -> Option<(u32, u32, Bytes)> {
        if let MessageType::Piece = self.message_type {
            if self.payload.len() >= 13 { // 4 (length) + 1 (id) + 4 (index) + 4 (begin)
                let mut cursor = Cursor::new(&self.payload[5..13]);
                let index = cursor.get_u32();
                let begin = cursor.get_u32();
                let data = Bytes::copy_from_slice(&self.payload[13..]);
                return Some((index, begin, data));
            }
        }
        None
    }

    /// Extract request details from a 'request' message
    pub fn get_request_details(&self) -> Option<(u32, u32, u32)> {
        if let MessageType::Request = self.message_type {
            if self.payload.len() >= 17 { // 4 (length) + 1 (id) + 4 (index) + 4 (begin) + 4 (length)
                let mut cursor = Cursor::new(&self.payload[5..17]);
                let index = cursor.get_u32();
                let begin = cursor.get_u32();
                let length = cursor.get_u32();
                return Some((index, begin, length));
            }
        }
        None
    }

    /// Write message to a stream
    pub async fn write_to<W: AsyncWriteExt + Unpin>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(&self.payload).await
    }
}