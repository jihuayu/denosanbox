//! Versioned, bounded byte framing plus typed error encoding.

use crate::error::{Error, ErrorKind, Result};

/// Wire magic.
pub const MAGIC: [u8; 4] = *b"DSB1";
/// Current protocol version.
pub const PROTOCOL_VERSION: u16 = 1;
/// Hard cap for a single frame payload.
pub const MAX_PAYLOAD: usize = 1024 * 1024;
/// Fixed header size: magic(4) version(2) kind(1) reserved(1) id(8) len(4).
pub const HEADER_LEN: usize = 20;

/// Message class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    /// Guest→host request.
    Request = 1,
    /// Host→guest response.
    Response = 2,
    /// Typed error response.
    Error = 3,
    /// Cooperative cancellation notice.
    Cancel = 4,
    /// Health check.
    Ping = 5,
}

impl FrameKind {
    fn from_byte(b: u8) -> Result<Self> {
        Ok(match b {
            1 => Self::Request,
            2 => Self::Response,
            3 => Self::Error,
            4 => Self::Cancel,
            5 => Self::Ping,
            _ => return Err(Error::protocol("unknown frame kind")),
        })
    }
}

/// One protocol frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    version: u16,
    kind: FrameKind,
    request_id: u64,
    payload: Vec<u8>,
}

impl Frame {
    /// Frame at the current protocol version.
    #[must_use]
    pub fn new(kind: FrameKind, request_id: u64, payload: Vec<u8>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            kind,
            request_id,
            payload,
        }
    }

    /// Wire version.
    #[must_use]
    pub fn version(&self) -> u16 {
        self.version
    }
    /// Message class.
    #[must_use]
    pub fn kind(&self) -> FrameKind {
        self.kind
    }
    /// Correlation id.
    #[must_use]
    pub fn request_id(&self) -> u64 {
        self.request_id
    }
    /// Payload bytes.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Serialize, enforcing `max_payload`.
    pub fn encode(&self, max_payload: usize) -> Result<Vec<u8>> {
        if self.payload.len() > max_payload.min(MAX_PAYLOAD) {
            return Err(Error::protocol("payload exceeds frame limit"));
        }
        let len =
            u32::try_from(self.payload.len()).map_err(|_| Error::protocol("payload too large"))?;
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&self.version.to_be_bytes());
        out.push(self.kind as u8);
        out.push(0);
        out.extend_from_slice(&self.request_id.to_be_bytes());
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    /// Parse exactly one frame; trailing bytes are rejected.
    pub fn decode(bytes: &[u8], max_payload: usize) -> Result<Frame> {
        if bytes.len() < HEADER_LEN {
            return Err(Error::protocol("truncated frame header"));
        }
        if bytes[..4] != MAGIC {
            return Err(Error::protocol("bad frame magic"));
        }
        let version = u16::from_be_bytes([bytes[4], bytes[5]]);
        if version != PROTOCOL_VERSION {
            return Err(Error::protocol("unsupported protocol version"));
        }
        let kind = FrameKind::from_byte(bytes[6])?;
        if bytes[7] != 0 {
            return Err(Error::protocol("reserved byte must be zero"));
        }
        let request_id = u64::from_be_bytes(bytes[8..16].try_into().expect("slice length"));
        let len = u32::from_be_bytes(bytes[16..20].try_into().expect("slice length")) as usize;
        if len > max_payload.min(MAX_PAYLOAD) {
            return Err(Error::protocol("payload exceeds frame limit"));
        }
        if bytes.len() != HEADER_LEN + len {
            return Err(Error::protocol("frame length mismatch"));
        }
        Ok(Frame {
            version,
            kind,
            request_id,
            payload: bytes[HEADER_LEN..].to_vec(),
        })
    }
}

fn sanitize(message: &str) -> String {
    let cleaned: String = message
        .chars()
        .take(512)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.is_empty() {
        "error".to_string()
    } else {
        cleaned
    }
}

/// Encode a typed error as payload: code(2) len(2) utf8 message.
#[must_use]
pub fn encode_error(err: &Error) -> Vec<u8> {
    let msg = sanitize(err.message());
    let bytes = msg.as_bytes();
    let len = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&err.kind().code().to_be_bytes());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&bytes[..usize::from(len)]);
    out
}

/// Decode an error payload, rejecting unknown codes and malformed text.
pub fn decode_error(payload: &[u8]) -> Result<Error> {
    if payload.len() < 4 {
        return Err(Error::protocol("truncated error payload"));
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    let len = u16::from_be_bytes([payload[2], payload[3]]) as usize;
    let kind = ErrorKind::from_code(code).ok_or_else(|| Error::protocol("unknown error code"))?;
    if payload.len() != 4 + len {
        return Err(Error::protocol("error length mismatch"));
    }
    let message = std::str::from_utf8(&payload[4..])
        .map_err(|_| Error::protocol("error message not utf8"))?;
    Ok(Error::new(kind, sanitize(message)))
}

/// Host handshake payload announcing the wire version and crate version.
#[must_use]
pub fn handshake_payload() -> Vec<u8> {
    format!("{}:{}", PROTOCOL_VERSION, crate::VERSION).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let f = Frame::new(FrameKind::Request, 42, b"hello".to_vec());
        let bytes = f.encode(1024).unwrap();
        assert_eq!(bytes.len(), HEADER_LEN + 5);
        let back = Frame::decode(&bytes, 1024).unwrap();
        assert_eq!(back, f);
        assert_eq!(back.version(), PROTOCOL_VERSION);
    }

    #[test]
    fn malformed_frames_rejected() {
        let f = Frame::new(FrameKind::Ping, 1, vec![]);
        let bytes = f.encode(1024).unwrap();
        assert!(Frame::decode(&bytes[..10], 1024).is_err());
        assert!(Frame::decode(&[bytes.clone(), vec![0]].concat(), 1024).is_err());
        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        assert!(Frame::decode(&bad_magic, 1024).is_err());
        let mut bad_ver = bytes.clone();
        bad_ver[4] = 9;
        assert!(Frame::decode(&bad_ver, 1024).is_err());
        let mut bad_kind = bytes.clone();
        bad_kind[6] = 99;
        assert!(Frame::decode(&bad_kind, 1024).is_err());
        assert!(Frame::new(FrameKind::Request, 1, vec![0; 10])
            .encode(9)
            .is_err());
    }

    #[test]
    fn typed_error_roundtrip_and_sanitization() {
        let e = Error::network_denied("bad\nhost");
        let payload = encode_error(&e);
        let back = decode_error(&payload).unwrap();
        assert_eq!(back.kind(), ErrorKind::NetworkDenied);
        assert!(!back.message().contains('\n'));
        assert!(decode_error(&[0, 99]).is_err());
        assert!(decode_error(&payload[..3]).is_err());
        assert!(handshake_payload().starts_with(b"1:"));
    }
}
