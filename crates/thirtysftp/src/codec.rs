//! Reading and writing whole packets on a byte stream.
//!
//! [`FrameReader`] and [`FrameWriter`] keep all of their progress inside the
//! struct, so dropping a pending [`read_frame`](FrameReader::read_frame) or
//! [`flush`](FrameWriter::flush) future (for example in a `select!`) loses no
//! data and leaves the stream in a consistent state.

use std::fmt;
use std::io;

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::wire::{Encode, EncodeError};

/// The largest packet OpenSSH accepts and a reasonable default: 256 KiB.
pub const DEFAULT_MAX_FRAME: u32 = 256 * 1024;

/// A failure reading or writing packets.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The underlying stream failed.
    Io(io::Error),
    /// The stream ended in the middle of a packet.
    UnexpectedEof,
    /// A packet announced a length of zero, which cannot hold a type byte.
    EmptyFrame,
    /// A packet announced a length above the limit. Nothing beyond the length
    /// field was read or allocated.
    FrameTooLarge {
        /// The announced length.
        len: u32,
        /// The configured limit.
        max: u32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::UnexpectedEof => f.write_str("stream ended in the middle of a packet"),
            Self::EmptyFrame => f.write_str("packet has zero length"),
            Self::FrameTooLarge { len, max } => {
                write!(f, "packet length {len} exceeds the limit of {max}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Reads length-prefixed packets from a stream.
///
/// After an error other than [`Error::Io`] with a transient cause the stream
/// is out of sync and should be dropped.
#[derive(Debug)]
pub struct FrameReader<R> {
    inner: R,
    buf: BytesMut,
    max_len: u32,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// Wraps `inner`. Packets longer than `max_len` bytes (not counting the
    /// length field) are rejected.
    pub fn new(inner: R, max_len: u32) -> Self {
        Self {
            inner,
            buf: BytesMut::new(),
            max_len,
        }
    }

    /// Reads the next packet and returns its payload, without the length
    /// prefix. Returns `Ok(None)` if the stream ended cleanly between
    /// packets.
    ///
    /// Cancel safe: bytes already received stay buffered.
    pub async fn read_frame(&mut self) -> Result<Option<Bytes>, Error> {
        loop {
            let mut need = 4;
            if self.buf.len() >= 4 {
                let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
                if len == 0 {
                    return Err(Error::EmptyFrame);
                }
                if len > self.max_len {
                    return Err(Error::FrameTooLarge {
                        len,
                        max: self.max_len,
                    });
                }
                let total = 4 + len as usize;
                if self.buf.len() >= total {
                    self.buf.advance(4);
                    return Ok(Some(self.buf.split_to(len as usize).freeze()));
                }
                need = total;
            }
            self.buf.reserve(need - self.buf.len());
            if self.inner.read_buf(&mut self.buf).await? == 0 {
                return if self.buf.is_empty() {
                    Ok(None)
                } else {
                    Err(Error::UnexpectedEof)
                };
            }
        }
    }

    /// The wrapped stream. Bytes already buffered are not included.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

/// Writes packets to a stream through an internal queue.
///
/// [`queue`](Self::queue) never blocks; [`flush`](Self::flush) drains the
/// queue. The queue has no limit of its own, so the caller decides how much to
/// queue before flushing.
#[derive(Debug)]
pub struct FrameWriter<W> {
    inner: W,
    buf: BytesMut,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    /// Wraps `inner`.
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            buf: BytesMut::new(),
        }
    }

    /// Appends a packet to the queue.
    pub fn queue(&mut self, packet: &impl Encode) -> Result<(), EncodeError> {
        packet.encode_frame(&mut self.buf)
    }

    /// Number of queued bytes not yet written.
    pub fn queued(&self) -> usize {
        self.buf.len()
    }

    /// Writes everything queued and flushes the stream.
    ///
    /// Cancel safe: the queue shrinks only by what the stream accepted, so a
    /// later call continues where this one stopped.
    pub async fn flush(&mut self) -> io::Result<()> {
        while !self.buf.is_empty() {
            if self.inner.write_buf(&mut self.buf).await? == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
        }
        self.inner.flush().await
    }

    /// The wrapped stream. Queued bytes are discarded.
    pub fn into_inner(self) -> W {
        self.inner
    }
}
