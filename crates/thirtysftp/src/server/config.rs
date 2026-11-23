//! Limits and settings of a served connection.

use std::time::Duration;

use tokio::sync::Semaphore;

use crate::codec::DEFAULT_MAX_FRAME;

/// Packet bytes kept back from `max_packet` when sizing reads and writes: the
/// header of a data packet, with room to spare.
const PACKET_OVERHEAD: u32 = 1024;

/// Limits and settings of [`serve`](super::serve). Every limit has a conservative default.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ServerConfig {
    /// Longest packet accepted from the client, in bytes. A longer one ends
    /// the connection. Checked before any buffer for it is allocated.
    pub max_packet: u32,
    /// Most files and directories open at once.
    pub max_handles: usize,
    /// Most bytes returned for one read. Longer reads are answered with this
    /// much, which the protocol allows.
    pub max_read_length: u32,
    /// Longest write accepted. A longer one fails with a status.
    pub max_write_length: u32,
    /// Most entries in one directory reply.
    pub max_dir_entries: usize,
    /// Most requests being worked on or waiting to be written at once. When
    /// it is reached the server stops reading from the client until one
    /// finishes.
    pub max_pending: usize,
    /// How long the client may take to send its first packet. `None` waits
    /// for ever.
    pub handshake_timeout: Option<Duration>,
    /// How long the connection may sit with no request arriving and none
    /// being served before it is ended. `None`, the default, never ends an
    /// idle connection: interactive clients are idle for hours.
    pub idle_timeout: Option<Duration>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_packet: DEFAULT_MAX_FRAME,
            max_handles: 256,
            max_read_length: 128 * 1024,
            max_write_length: 128 * 1024,
            max_dir_entries: 100,
            max_pending: 128,
            handshake_timeout: Some(Duration::from_secs(60)),
            idle_timeout: None,
        }
    }
}

impl ServerConfig {
    /// Sets [`max_packet`](Self::max_packet).
    pub fn with_max_packet(mut self, bytes: u32) -> Self {
        self.max_packet = bytes;
        self
    }

    /// Sets [`max_handles`](Self::max_handles).
    pub fn with_max_handles(mut self, count: usize) -> Self {
        self.max_handles = count;
        self
    }

    /// Sets [`max_read_length`](Self::max_read_length).
    pub fn with_max_read_length(mut self, bytes: u32) -> Self {
        self.max_read_length = bytes;
        self
    }

    /// Sets [`max_write_length`](Self::max_write_length).
    pub fn with_max_write_length(mut self, bytes: u32) -> Self {
        self.max_write_length = bytes;
        self
    }

    /// Sets [`max_dir_entries`](Self::max_dir_entries).
    pub fn with_max_dir_entries(mut self, count: usize) -> Self {
        self.max_dir_entries = count;
        self
    }

    /// Sets [`max_pending`](Self::max_pending).
    pub fn with_max_pending(mut self, count: usize) -> Self {
        self.max_pending = count;
        self
    }

    /// Sets [`handshake_timeout`](Self::handshake_timeout).
    pub fn with_handshake_timeout(mut self, limit: Option<Duration>) -> Self {
        self.handshake_timeout = limit;
        self
    }

    /// Sets [`idle_timeout`](Self::idle_timeout).
    pub fn with_idle_timeout(mut self, limit: Option<Duration>) -> Self {
        self.idle_timeout = limit;
        self
    }

    /// The values actually used: nothing below 1, and data lengths that fit
    /// in a packet.
    pub(super) fn effective(&self) -> Self {
        let max_packet = self.max_packet.max(2 * PACKET_OVERHEAD);
        let room = max_packet - PACKET_OVERHEAD;
        Self {
            max_packet,
            max_handles: self.max_handles.max(1),
            max_read_length: self.max_read_length.clamp(1, room),
            max_write_length: self.max_write_length.clamp(1, room),
            max_dir_entries: self.max_dir_entries.max(1),
            max_pending: self.max_pending.clamp(1, Semaphore::MAX_PERMITS),
            handshake_timeout: self.handshake_timeout,
            idle_timeout: self.idle_timeout,
        }
    }
}
