//! Bounds-checked reading of SFTP primitives from one packet.

use bytes::{Buf, Bytes};

use super::DecodeError;

/// A cursor over the payload of a single packet. Every read checks the
/// remaining length first, so malformed input yields an error, never a panic.
pub(crate) struct Reader(Bytes);

impl Reader {
    pub(crate) fn new(payload: Bytes) -> Self {
        Self(payload)
    }

    pub(crate) fn remaining(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn u8(&mut self) -> Result<u8, DecodeError> {
        if self.0.is_empty() {
            return Err(DecodeError::Truncated);
        }
        Ok(self.0.get_u8())
    }

    pub(crate) fn u32(&mut self) -> Result<u32, DecodeError> {
        if self.0.len() < 4 {
            return Err(DecodeError::Truncated);
        }
        Ok(self.0.get_u32())
    }

    pub(crate) fn u64(&mut self) -> Result<u64, DecodeError> {
        if self.0.len() < 8 {
            return Err(DecodeError::Truncated);
        }
        Ok(self.0.get_u64())
    }

    /// A length-prefixed byte string. The result shares the packet's buffer.
    pub(crate) fn string(&mut self) -> Result<Bytes, DecodeError> {
        let len = self.u32()? as usize;
        if self.0.len() < len {
            return Err(DecodeError::Truncated);
        }
        Ok(self.0.split_to(len))
    }

    /// Everything that is left.
    pub(crate) fn rest(&mut self) -> Bytes {
        self.0.split_to(self.0.len())
    }

    /// Fails if any bytes are left.
    pub(crate) fn finish(&self) -> Result<(), DecodeError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingData)
        }
    }
}
