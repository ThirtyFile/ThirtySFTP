//! The protocol operations, one method per request type.
//!
//! Paths and data are byte strings. Reads that hit the end of the file or
//! directory return `Ok(None)`; every other non-success status becomes
//! [`Error::Status`].

use bytes::Bytes;

use super::{Handle, Session};
use crate::error::{Error, ProtocolError, Status};
use crate::wire::{Attributes, NameEntry, Op, OpenFlags, Reply, StatusCode};

pub(super) fn bytes(s: impl AsRef<[u8]>) -> Bytes {
    Bytes::copy_from_slice(s.as_ref())
}

/// The error for a reply that is not the one the request calls for. A status
/// reply becomes the server's error; success where data was due is a
/// protocol violation.
pub(super) fn unexpected(reply: Reply, wanted: &'static str) -> Error {
    match reply {
        Reply::Status { code, message, .. } if code != StatusCode::Ok => Error::Status(Status {
            code,
            message: String::from_utf8_lossy(&message).into_owned(),
        }),
        _ => Error::Protocol(ProtocolError::UnexpectedReply(wanted)),
    }
}

pub(super) fn is_eof(reply: &Reply) -> bool {
    matches!(
        reply,
        Reply::Status {
            code: StatusCode::Eof,
            ..
        }
    )
}

/// Checks the reply to a request that returns no data.
pub(super) fn check_ok(reply: Reply) -> Result<(), Error> {
    match reply {
        Reply::Status {
            code: StatusCode::Ok,
            ..
        } => Ok(()),
        other => Err(unexpected(other, "status")),
    }
}

/// Checks the reply to a read of up to `len` bytes: `None` at the end of the
/// file, otherwise data that is not longer than asked for.
pub(super) fn check_read(reply: Reply, len: u32) -> Result<Option<Bytes>, Error> {
    match reply {
        Reply::Data(data) if data.len() > len as usize => {
            Err(Error::Protocol(ProtocolError::OversizedData))
        }
        Reply::Data(data) => Ok(Some(data)),
        other if is_eof(&other) => Ok(None),
        other => Err(unexpected(other, "data")),
    }
}

impl Session {
    async fn expect_ok(&self, op: Op) -> Result<(), Error> {
        check_ok(self.request(op).await?)
    }

    async fn expect_attrs(&self, op: Op) -> Result<Attributes, Error> {
        match self.request(op).await? {
            Reply::Attrs(attrs) => Ok(attrs),
            other => Err(unexpected(other, "attributes")),
        }
    }

    async fn expect_handle(&self, op: Op) -> Result<Handle, Error> {
        match self.request(op).await? {
            Reply::Handle(handle) => Ok(Handle(handle)),
            other => Err(unexpected(other, "handle")),
        }
    }

    /// A reply that holds exactly one name: `realpath` and `readlink`.
    pub(super) async fn expect_one_name(&self, op: Op) -> Result<Bytes, Error> {
        match self.request(op).await? {
            Reply::Name(mut names) if names.len() == 1 => Ok(names.remove(0).name),
            Reply::Name(_) => Err(Error::Protocol(ProtocolError::UnexpectedReply(
                "exactly one name",
            ))),
            other => Err(unexpected(other, "name")),
        }
    }

    /// Opens a file.
    pub async fn open(
        &self,
        path: impl AsRef<[u8]>,
        flags: OpenFlags,
        attrs: Attributes,
    ) -> Result<Handle, Error> {
        self.expect_handle(Op::Open {
            path: bytes(path),
            flags,
            attrs,
        })
        .await
    }

    /// Closes a file or directory handle.
    pub async fn close_handle(&self, handle: &Handle) -> Result<(), Error> {
        self.expect_ok(Op::Close {
            handle: handle.0.clone(),
        })
        .await
    }

    /// Reads up to `len` bytes at `offset`. Returns `None` at the end of the
    /// file. The server may return fewer bytes than asked for, even before
    /// the end; a reply with more is a protocol violation.
    pub async fn read(
        &self,
        handle: &Handle,
        offset: u64,
        len: u32,
    ) -> Result<Option<Bytes>, Error> {
        let reply = self
            .request(Op::Read {
                handle: handle.0.clone(),
                offset,
                len,
            })
            .await?;
        check_read(reply, len)
    }

    /// Writes `data` at `offset`.
    pub async fn write(
        &self,
        handle: &Handle,
        offset: u64,
        data: impl Into<Bytes>,
    ) -> Result<(), Error> {
        self.expect_ok(Op::Write {
            handle: handle.0.clone(),
            offset,
            data: data.into(),
        })
        .await
    }

    /// Attributes of `path`, following symbolic links.
    pub async fn stat(&self, path: impl AsRef<[u8]>) -> Result<Attributes, Error> {
        self.expect_attrs(Op::Stat { path: bytes(path) }).await
    }

    /// Attributes of `path`, not following a symbolic link at the end.
    pub async fn lstat(&self, path: impl AsRef<[u8]>) -> Result<Attributes, Error> {
        self.expect_attrs(Op::Lstat { path: bytes(path) }).await
    }

    /// Attributes of an open file.
    pub async fn fstat(&self, handle: &Handle) -> Result<Attributes, Error> {
        self.expect_attrs(Op::Fstat {
            handle: handle.0.clone(),
        })
        .await
    }

    /// Sets attributes of `path`.
    pub async fn setstat(&self, path: impl AsRef<[u8]>, attrs: Attributes) -> Result<(), Error> {
        self.expect_ok(Op::SetStat {
            path: bytes(path),
            attrs,
        })
        .await
    }

    /// Sets attributes of an open file.
    pub async fn fsetstat(&self, handle: &Handle, attrs: Attributes) -> Result<(), Error> {
        self.expect_ok(Op::FsetStat {
            handle: handle.0.clone(),
            attrs,
        })
        .await
    }

    /// Opens a directory for listing.
    pub async fn opendir(&self, path: impl AsRef<[u8]>) -> Result<Handle, Error> {
        self.expect_handle(Op::OpenDir { path: bytes(path) }).await
    }

    /// Reads the next batch of entries. Returns `None` when the listing is
    /// complete. The batch includes `.` and `..` if the server lists them.
    pub async fn readdir(&self, handle: &Handle) -> Result<Option<Vec<NameEntry>>, Error> {
        let reply = self
            .request(Op::ReadDir {
                handle: handle.0.clone(),
            })
            .await?;
        match reply {
            Reply::Name(entries) => Ok(Some(entries)),
            other if is_eof(&other) => Ok(None),
            other => Err(unexpected(other, "name")),
        }
    }

    /// Removes a file.
    pub async fn remove(&self, path: impl AsRef<[u8]>) -> Result<(), Error> {
        self.expect_ok(Op::Remove { path: bytes(path) }).await
    }

    /// Creates a directory.
    pub async fn mkdir(&self, path: impl AsRef<[u8]>, attrs: Attributes) -> Result<(), Error> {
        self.expect_ok(Op::MkDir {
            path: bytes(path),
            attrs,
        })
        .await
    }

    /// Removes a directory.
    pub async fn rmdir(&self, path: impl AsRef<[u8]>) -> Result<(), Error> {
        self.expect_ok(Op::RmDir { path: bytes(path) }).await
    }

    /// The canonical absolute form of `path`.
    pub async fn realpath(&self, path: impl AsRef<[u8]>) -> Result<Bytes, Error> {
        self.expect_one_name(Op::RealPath { path: bytes(path) })
            .await
    }

    /// Renames `from` to `to`. Version 3 does not say whether an existing
    /// `to` is replaced; servers differ.
    pub async fn rename(&self, from: impl AsRef<[u8]>, to: impl AsRef<[u8]>) -> Result<(), Error> {
        self.expect_ok(Op::Rename {
            from: bytes(from),
            to: bytes(to),
        })
        .await
    }

    /// The target of the symbolic link `path`.
    pub async fn readlink(&self, path: impl AsRef<[u8]>) -> Result<Bytes, Error> {
        self.expect_one_name(Op::ReadLink { path: bytes(path) })
            .await
    }

    /// Creates the symbolic link `link` pointing at `target`.
    pub async fn symlink(
        &self,
        target: impl AsRef<[u8]>,
        link: impl AsRef<[u8]>,
    ) -> Result<(), Error> {
        self.expect_ok(Op::Symlink {
            target: bytes(target),
            link: bytes(link),
        })
        .await
    }

    /// Sends an extended request and returns the data of the extended reply.
    /// A plain success status, which some servers send for extensions that
    /// return nothing, gives empty data.
    pub async fn extended(
        &self,
        name: impl AsRef<[u8]>,
        data: impl Into<Bytes>,
    ) -> Result<Bytes, Error> {
        let reply = self
            .request(Op::Extended {
                name: bytes(name),
                data: data.into(),
            })
            .await?;
        match reply {
            Reply::Extended(data) => Ok(data),
            Reply::Status {
                code: StatusCode::Ok,
                ..
            } => Ok(Bytes::new()),
            other => Err(unexpected(other, "extended reply")),
        }
    }
}
