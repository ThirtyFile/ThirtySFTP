//! File attributes (`ATTRS`).

use bytes::{BufMut, BytesMut};

use super::packet::Extension;
use super::{DecodeError, EncodeError, Reader, put_string};

const SIZE: u32 = 0x0000_0001;
const UIDGID: u32 = 0x0000_0002;
const PERMISSIONS: u32 = 0x0000_0004;
const ACMODTIME: u32 = 0x0000_0008;
const EXTENDED: u32 = 0x8000_0000;
const KNOWN: u32 = SIZE | UIDGID | PERMISSIONS | ACMODTIME | EXTENDED;

/// Numeric owner and group of a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Owner {
    /// User id.
    pub uid: u32,
    /// Group id.
    pub gid: u32,
}

/// Access and modification time, in seconds since the Unix epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Times {
    /// Last access.
    pub accessed: u32,
    /// Last modification.
    pub modified: u32,
}

/// The kind of a file, from the type bits of its permissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileType {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link.
    Symlink,
    /// Anything else: devices, sockets, pipes.
    Other,
}

/// File attributes. Each field is optional; only the fields that are `Some`
/// are put on the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attributes {
    /// Size in bytes.
    pub size: Option<u64>,
    /// Owner and group.
    pub owner: Option<Owner>,
    /// POSIX permission and file-type bits.
    pub permissions: Option<u32>,
    /// Access and modification times.
    pub times: Option<Times>,
    /// Vendor extension pairs.
    pub extended: Vec<Extension>,
}

impl Attributes {
    /// The file type, if the permissions are present.
    pub fn file_type(&self) -> Option<FileType> {
        self.permissions.map(|mode| match mode & 0o170_000 {
            0o100_000 => FileType::File,
            0o040_000 => FileType::Dir,
            0o120_000 => FileType::Symlink,
            _ => FileType::Other,
        })
    }

    /// Whether the file is a directory. `false` if the type is unknown.
    pub fn is_dir(&self) -> bool {
        self.file_type() == Some(FileType::Dir)
    }

    /// Whether the file is a regular file. `false` if the type is unknown.
    pub fn is_file(&self) -> bool {
        self.file_type() == Some(FileType::File)
    }

    /// Whether the file is a symbolic link. `false` if the type is unknown.
    pub fn is_symlink(&self) -> bool {
        self.file_type() == Some(FileType::Symlink)
    }

    pub(crate) fn encode(&self, out: &mut BytesMut) -> Result<(), EncodeError> {
        let mut flags = 0;
        if self.size.is_some() {
            flags |= SIZE;
        }
        if self.owner.is_some() {
            flags |= UIDGID;
        }
        if self.permissions.is_some() {
            flags |= PERMISSIONS;
        }
        if self.times.is_some() {
            flags |= ACMODTIME;
        }
        if !self.extended.is_empty() {
            flags |= EXTENDED;
        }
        out.put_u32(flags);
        if let Some(size) = self.size {
            out.put_u64(size);
        }
        if let Some(owner) = self.owner {
            out.put_u32(owner.uid);
            out.put_u32(owner.gid);
        }
        if let Some(permissions) = self.permissions {
            out.put_u32(permissions);
        }
        if let Some(times) = self.times {
            out.put_u32(times.accessed);
            out.put_u32(times.modified);
        }
        if !self.extended.is_empty() {
            out.put_u32(u32::try_from(self.extended.len()).map_err(|_| EncodeError)?);
            for pair in &self.extended {
                put_string(out, &pair.name)?;
                put_string(out, &pair.data)?;
            }
        }
        Ok(())
    }

    pub(crate) fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        let flags = r.u32()?;
        if flags & !KNOWN != 0 {
            return Err(DecodeError::UnknownAttributeFlags(flags & !KNOWN));
        }
        let mut attrs = Self::default();
        if flags & SIZE != 0 {
            attrs.size = Some(r.u64()?);
        }
        if flags & UIDGID != 0 {
            attrs.owner = Some(Owner {
                uid: r.u32()?,
                gid: r.u32()?,
            });
        }
        if flags & PERMISSIONS != 0 {
            attrs.permissions = Some(r.u32()?);
        }
        if flags & ACMODTIME != 0 {
            attrs.times = Some(Times {
                accessed: r.u32()?,
                modified: r.u32()?,
            });
        }
        if flags & EXTENDED != 0 {
            let count = r.u32()? as usize;
            // Each pair takes at least two length fields.
            if count > r.remaining() / 8 {
                return Err(DecodeError::CountTooLarge);
            }
            attrs.extended.reserve_exact(count);
            for _ in 0..count {
                attrs.extended.push(Extension {
                    name: r.string()?,
                    data: r.string()?,
                });
            }
        }
        Ok(attrs)
    }
}
