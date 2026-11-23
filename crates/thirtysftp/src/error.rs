//! The error type shared by the client and the server.

use std::fmt;
use std::io;
use std::sync::Arc;

use crate::codec;
use crate::wire::{DecodeError, EncodeError, StatusCode};

/// A non-success `STATUS` reply from the peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The result code. Match on this rather than on the message.
    pub code: StatusCode,
    /// The peer's message, converted lossily to UTF-8.
    pub message: String,
}

/// How the peer broke the protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolError {
    /// A packet could not be decoded.
    Decode(DecodeError),
    /// A packet announced a length of zero.
    EmptyFrame,
    /// A packet announced a length above the configured limit.
    FrameTooLarge {
        /// The announced length.
        len: u32,
        /// The configured limit.
        max: u32,
    },
    /// A reply carried a request id that has no request waiting.
    UnknownRequestId(u32),
    /// A reply had the wrong type for the request. The text names what was
    /// expected.
    UnexpectedReply(&'static str),
    /// A handshake packet arrived after the handshake.
    UnexpectedHandshake,
    /// The peer chose a protocol version this crate does not speak.
    UnsupportedVersion(u32),
    /// A read returned more data than was asked for.
    OversizedData,
    /// A read returned no data, but not the end of the file either, so
    /// repeating it would never finish.
    NoProgress,
}

/// A failure of an SFTP operation.
///
/// The type is `Clone` so that one connection failure can be reported to
/// every request that was waiting on it.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The peer answered with a non-success status.
    Status(Status),
    /// The peer broke the protocol. The connection is closed.
    Protocol(ProtocolError),
    /// The underlying stream failed. The connection is closed.
    Io(Arc<io::Error>),
    /// The peer closed the connection.
    Disconnected,
    /// The session was closed on this side.
    Closed,
    /// The server did not announce the extension this call needs. Nothing
    /// was sent.
    Unsupported(&'static str),
    /// What was asked for cannot be put into a packet: a path, some data or
    /// a list is longer than the protocol's 32-bit lengths allow. Nothing
    /// was sent, and the session is unaffected.
    Encode(EncodeError),
}

impl Error {
    /// The status code, if the peer answered with one.
    pub fn status_code(&self) -> Option<StatusCode> {
        match self {
            Self::Status(s) => Some(s.code),
            _ => None,
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "undecodable packet: {e}"),
            Self::EmptyFrame => f.write_str("packet has zero length"),
            Self::FrameTooLarge { len, max } => {
                write!(f, "packet length {len} exceeds the limit of {max}")
            }
            Self::UnknownRequestId(id) => write!(f, "reply to unknown request id {id}"),
            Self::UnexpectedReply(want) => write!(f, "unexpected reply, wanted {want}"),
            Self::UnexpectedHandshake => f.write_str("handshake packet after the handshake"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported protocol version {v}"),
            Self::OversizedData => f.write_str("read returned more data than requested"),
            Self::NoProgress => f.write_str("read made no progress"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status(s) => write!(f, "server returned {:?}: {}", s.code, s.message),
            Self::Protocol(e) => write!(f, "protocol violation: {e}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Disconnected => f.write_str("connection closed by the peer"),
            Self::Closed => f.write_str("session closed"),
            Self::Unsupported(name) => write!(f, "server does not support {name}"),
            Self::Encode(e) => write!(f, "request cannot be encoded: {e}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e.as_ref()),
            Self::Protocol(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(Arc::new(e))
    }
}

impl From<codec::Error> for Error {
    fn from(e: codec::Error) -> Self {
        match e {
            codec::Error::Io(e) => e.into(),
            codec::Error::UnexpectedEof => io::Error::from(io::ErrorKind::UnexpectedEof).into(),
            codec::Error::EmptyFrame => Self::Protocol(ProtocolError::EmptyFrame),
            codec::Error::FrameTooLarge { len, max } => {
                Self::Protocol(ProtocolError::FrameTooLarge { len, max })
            }
        }
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        let kind = match &e {
            Error::Status(s) => match s.code {
                StatusCode::Eof => io::ErrorKind::UnexpectedEof,
                StatusCode::NoSuchFile => io::ErrorKind::NotFound,
                StatusCode::PermissionDenied => io::ErrorKind::PermissionDenied,
                StatusCode::BadMessage => io::ErrorKind::InvalidData,
                StatusCode::NoConnection | StatusCode::ConnectionLost => {
                    io::ErrorKind::ConnectionAborted
                }
                StatusCode::OpUnsupported => io::ErrorKind::Unsupported,
                _ => io::ErrorKind::Other,
            },
            Error::Protocol(_) => io::ErrorKind::InvalidData,
            Error::Io(e) => e.kind(),
            Error::Disconnected => io::ErrorKind::ConnectionAborted,
            Error::Closed => io::ErrorKind::NotConnected,
            Error::Unsupported(_) => io::ErrorKind::Unsupported,
            Error::Encode(_) => io::ErrorKind::InvalidInput,
        };
        Self::new(kind, e)
    }
}
