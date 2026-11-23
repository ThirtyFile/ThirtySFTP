//! Extension names and the data types shared by client and server.
//!
//! The names are as announced in the handshake. The types are the payloads
//! of the extensions that carry structured data.

use bytes::{BufMut, Bytes, BytesMut};

use crate::wire::{DecodeError, Reader};

/// `posix-rename@openssh.com`: rename that replaces an existing target.
pub const POSIX_RENAME: &str = "posix-rename@openssh.com";
/// `statvfs@openssh.com`: file system statistics by path.
pub const STATVFS: &str = "statvfs@openssh.com";
/// `fstatvfs@openssh.com`: file system statistics by handle.
pub const FSTATVFS: &str = "fstatvfs@openssh.com";
/// `hardlink@openssh.com`: create a hard link.
pub const HARDLINK: &str = "hardlink@openssh.com";
/// `fsync@openssh.com`: flush a file to stable storage.
pub const FSYNC: &str = "fsync@openssh.com";
/// `lsetstat@openssh.com`: set attributes without following a link.
pub const LSETSTAT: &str = "lsetstat@openssh.com";
/// `limits@openssh.com`: the sizes the server accepts.
pub const LIMITS: &str = "limits@openssh.com";
/// `expand-path@openssh.com`: expand `~` and relative paths on the server.
pub const EXPAND_PATH: &str = "expand-path@openssh.com";
/// `copy-data`: copy between open files on the server.
pub const COPY_DATA: &str = "copy-data";
/// `home-directory`: the home directory of a user.
pub const HOME_DIRECTORY: &str = "home-directory";
/// `users-groups-by-id@openssh.com`: user and group names by numeric id.
pub const USERS_GROUPS_BY_ID: &str = "users-groups-by-id@openssh.com";

/// What the server says about the sizes it accepts (`limits@openssh.com`).
/// A value of 0 means the server states no limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Longest packet, length field excluded.
    pub max_packet_length: u64,
    /// Most bytes in one read request.
    pub max_read_length: u64,
    /// Most bytes in one write request.
    pub max_write_length: u64,
    /// Most handles open at once.
    pub max_open_handles: u64,
}

impl Limits {
    /// Decodes the data of an extended reply.
    pub fn decode(data: Bytes) -> Result<Self, DecodeError> {
        let mut r = Reader::new(data);
        let limits = Self {
            max_packet_length: r.u64()?,
            max_read_length: r.u64()?,
            max_write_length: r.u64()?,
            max_open_handles: r.u64()?,
        };
        r.finish()?;
        Ok(limits)
    }

    /// Encodes the data of an extended reply.
    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(32);
        out.put_u64(self.max_packet_length);
        out.put_u64(self.max_read_length);
        out.put_u64(self.max_write_length);
        out.put_u64(self.max_open_handles);
        out.freeze()
    }
}

/// File system statistics (`statvfs@openssh.com`), as in POSIX `statvfs`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatVfs {
    /// File system block size.
    pub block_size: u64,
    /// Fundamental block size.
    pub fragment_size: u64,
    /// Total blocks, in units of `fragment_size`.
    pub blocks: u64,
    /// Free blocks.
    pub blocks_free: u64,
    /// Free blocks for unprivileged users.
    pub blocks_available: u64,
    /// Total inodes.
    pub files: u64,
    /// Free inodes.
    pub files_free: u64,
    /// Free inodes for unprivileged users.
    pub files_available: u64,
    /// File system id.
    pub id: u64,
    /// Mount flags; see [`READ_ONLY`](Self::READ_ONLY) and
    /// [`NO_SUID`](Self::NO_SUID).
    pub flags: u64,
    /// Longest file name.
    pub name_max: u64,
}

impl StatVfs {
    /// Flag: mounted read-only.
    pub const READ_ONLY: u64 = 1;
    /// Flag: set-user-id bits are ignored.
    pub const NO_SUID: u64 = 2;

    /// Decodes the data of an extended reply.
    pub fn decode(data: Bytes) -> Result<Self, DecodeError> {
        let mut r = Reader::new(data);
        let v = Self {
            block_size: r.u64()?,
            fragment_size: r.u64()?,
            blocks: r.u64()?,
            blocks_free: r.u64()?,
            blocks_available: r.u64()?,
            files: r.u64()?,
            files_free: r.u64()?,
            files_available: r.u64()?,
            id: r.u64()?,
            flags: r.u64()?,
            name_max: r.u64()?,
        };
        r.finish()?;
        Ok(v)
    }

    /// Encodes the data of an extended reply.
    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(88);
        for v in [
            self.block_size,
            self.fragment_size,
            self.blocks,
            self.blocks_free,
            self.blocks_available,
            self.files,
            self.files_free,
            self.files_available,
            self.id,
            self.flags,
            self.name_max,
        ] {
            out.put_u64(v);
        }
        out.freeze()
    }
}
