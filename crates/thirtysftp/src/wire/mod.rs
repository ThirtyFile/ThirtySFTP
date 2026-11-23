//! Encoding and decoding of SFTP version 3 packets.
//!
//! Everything here is pure: no I/O, and no allocation sized by an untrusted
//! length field. Decoding takes the payload of one packet (the bytes after the
//! 4-byte length prefix) and returns either a typed packet or a
//! [`DecodeError`]. Encoding writes a complete frame, length prefix included.
//!
//! Paths, handles, messages and data are byte strings ([`bytes::Bytes`]),
//! because the protocol does not require file names to be UTF-8. Decoded
//! strings share the buffer of the packet they came from.

use std::fmt;

use bytes::{BufMut, BytesMut};

mod attrs;
mod packet;
mod reader;

pub use attrs::{Attributes, FileType, Owner, Times};
pub use packet::{
    ClientPacket, Extension, Hello, NameEntry, Op, OpenFlags, Reply, Request, Response,
    ServerPacket, StatusCode,
};
pub(crate) use reader::Reader;

/// Why a packet could not be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The packet ended before a field was complete.
    Truncated,
    /// Bytes were left over after the last field.
    TrailingData,
    /// A count field claims more entries than the packet could possibly hold.
    CountTooLarge,
    /// The attribute flags contain bits this implementation cannot parse.
    UnknownAttributeFlags(u32),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("packet is truncated"),
            Self::TrailingData => f.write_str("packet has trailing data"),
            Self::CountTooLarge => f.write_str("entry count exceeds the packet size"),
            Self::UnknownAttributeFlags(bits) => {
                write!(f, "unknown attribute flags {bits:#x}")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// A field or packet is too long for the 32-bit length the protocol uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeError;

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("field is too long for the protocol")
    }
}

impl std::error::Error for EncodeError {}

/// Writes a length-prefixed byte string.
pub(crate) fn put_string(out: &mut BytesMut, s: &[u8]) -> Result<(), EncodeError> {
    let len = u32::try_from(s.len()).map_err(|_| EncodeError)?;
    out.put_u32(len);
    out.extend_from_slice(s);
    Ok(())
}

/// Writes a frame: a 4-byte length followed by whatever `body` appends. On
/// error `out` is left as it was.
pub(crate) fn frame(
    out: &mut BytesMut,
    body: impl FnOnce(&mut BytesMut) -> Result<(), EncodeError>,
) -> Result<(), EncodeError> {
    let start = out.len();
    out.put_u32(0);
    if let Err(e) = body(out) {
        out.truncate(start);
        return Err(e);
    }
    match u32::try_from(out.len() - start - 4) {
        Ok(len) => {
            out[start..start + 4].copy_from_slice(&len.to_be_bytes());
            Ok(())
        }
        Err(_) => {
            out.truncate(start);
            Err(EncodeError)
        }
    }
}

/// A packet that can be written as one frame.
pub trait Encode {
    /// Appends the packet to `out`, length prefix included. On error `out` is
    /// unchanged.
    fn encode_frame(&self, out: &mut BytesMut) -> Result<(), EncodeError>;
}
