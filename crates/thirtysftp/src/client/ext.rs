//! Typed access to the OpenSSH and draft extensions.
//!
//! Each method first checks that the server announced the extension in its
//! handshake and returns [`Error::Unsupported`] without sending anything if
//! it did not. Use [`Session::supports`] to ask in advance.

use bytes::{BufMut, Bytes, BytesMut};

use super::ops::{bytes, check_ok, unexpected};
use super::{Handle, Session};
use crate::error::{Error, ProtocolError};
use crate::wire::{Attributes, Op, Reader, Reply, put_string};

pub(super) use crate::extensions::LIMITS;
use crate::extensions::{
    COPY_DATA, EXPAND_PATH, FSTATVFS, FSYNC, HARDLINK, HOME_DIRECTORY, LSETSTAT, Limits,
    POSIX_RENAME, STATVFS, StatVfs, USERS_GROUPS_BY_ID,
};

/// Decodes the reply to a `limits@openssh.com` request.
pub(super) fn limits_from_reply(reply: Reply) -> Result<Limits, Error> {
    let Reply::Extended(data) = reply else {
        return Err(unexpected(reply, "limits"));
    };
    Limits::decode(data).map_err(|e| Error::Protocol(ProtocolError::Decode(e)))
}

fn statvfs_from_reply(reply: Reply) -> Result<StatVfs, Error> {
    let Reply::Extended(data) = reply else {
        return Err(unexpected(reply, "statvfs"));
    };
    StatVfs::decode(data).map_err(|e| Error::Protocol(ProtocolError::Decode(e)))
}

/// Parses a packed array of length-prefixed strings.
fn strings(data: Bytes) -> Result<Vec<Bytes>, Error> {
    let mut r = Reader::new(data);
    let mut out = Vec::new();
    while r.remaining() > 0 {
        out.push(
            r.string()
                .map_err(|e| Error::Protocol(ProtocolError::Decode(e)))?,
        );
    }
    Ok(out)
}

impl Session {
    fn require(&self, name: &'static str) -> Result<(), Error> {
        if self.supports(name) {
            Ok(())
        } else {
            Err(Error::Unsupported(name))
        }
    }

    /// Sends an extended request whose data is built by `build`, after
    /// checking that the server announced `name`.
    async fn ext_request(
        &self,
        name: &'static str,
        build: impl FnOnce(&mut BytesMut) -> Result<(), crate::wire::EncodeError>,
    ) -> Result<Reply, Error> {
        self.require(name)?;
        let mut data = BytesMut::new();
        build(&mut data).map_err(Error::Encode)?;
        self.request(Op::Extended {
            name: Bytes::from_static(name.as_bytes()),
            data: data.freeze(),
        })
        .await
    }

    /// The limits the server states. Cached from the handshake when
    /// available; otherwise asked for.
    pub async fn limits(&self) -> Result<Limits, Error> {
        if let Some(limits) = self.known_limits() {
            return Ok(*limits);
        }
        self.require(LIMITS)?;
        self.fetch_limits().await
    }

    /// Renames `from` to `to`, replacing `to` if it exists (POSIX `rename`).
    pub async fn posix_rename(
        &self,
        from: impl AsRef<[u8]>,
        to: impl AsRef<[u8]>,
    ) -> Result<(), Error> {
        let (from, to) = (bytes(from), bytes(to));
        let reply = self
            .ext_request(POSIX_RENAME, |out| {
                put_string(out, &from)?;
                put_string(out, &to)
            })
            .await?;
        check_ok(reply)
    }

    /// Creates the hard link `new` to the existing file `existing`.
    pub async fn hardlink(
        &self,
        existing: impl AsRef<[u8]>,
        new: impl AsRef<[u8]>,
    ) -> Result<(), Error> {
        let (existing, new) = (bytes(existing), bytes(new));
        let reply = self
            .ext_request(HARDLINK, |out| {
                put_string(out, &existing)?;
                put_string(out, &new)
            })
            .await?;
        check_ok(reply)
    }

    /// Flushes an open file to stable storage.
    pub async fn fsync(&self, handle: &Handle) -> Result<(), Error> {
        let reply = self
            .ext_request(FSYNC, |out| put_string(out, &handle.0))
            .await?;
        check_ok(reply)
    }

    /// Sets attributes of `path` without following a symbolic link at the
    /// end.
    pub async fn lsetstat(&self, path: impl AsRef<[u8]>, attrs: &Attributes) -> Result<(), Error> {
        let path = bytes(path);
        let reply = self
            .ext_request(LSETSTAT, |out| {
                put_string(out, &path)?;
                attrs.encode(out)
            })
            .await?;
        check_ok(reply)
    }

    /// Statistics of the file system holding `path`.
    pub async fn statvfs(&self, path: impl AsRef<[u8]>) -> Result<StatVfs, Error> {
        let path = bytes(path);
        let reply = self
            .ext_request(STATVFS, |out| put_string(out, &path))
            .await?;
        statvfs_from_reply(reply)
    }

    /// Statistics of the file system holding an open file.
    pub async fn fstatvfs(&self, handle: &Handle) -> Result<StatVfs, Error> {
        let reply = self
            .ext_request(FSTATVFS, |out| put_string(out, &handle.0))
            .await?;
        statvfs_from_reply(reply)
    }

    /// Expands `path` on the server: `~`, `~user` and relative paths.
    pub async fn expand_path(&self, path: impl AsRef<[u8]>) -> Result<Bytes, Error> {
        let path = bytes(path);
        let reply = self
            .ext_request(EXPAND_PATH, |out| put_string(out, &path))
            .await?;
        single_name(reply)
    }

    /// The home directory of `user`; the empty name means the current user.
    pub async fn home_directory(&self, user: impl AsRef<[u8]>) -> Result<Bytes, Error> {
        let user = bytes(user);
        let reply = self
            .ext_request(HOME_DIRECTORY, |out| put_string(out, &user))
            .await?;
        single_name(reply)
    }

    /// Copies `len` bytes from one open file to another on the server,
    /// without moving the data through the client. A `len` of 0 copies up to
    /// the end of the source.
    pub async fn copy_data(
        &self,
        source: &Handle,
        source_offset: u64,
        len: u64,
        destination: &Handle,
        destination_offset: u64,
    ) -> Result<(), Error> {
        let reply = self
            .ext_request(COPY_DATA, |out| {
                put_string(out, &source.0)?;
                out.put_u64(source_offset);
                out.put_u64(len);
                put_string(out, &destination.0)?;
                out.put_u64(destination_offset);
                Ok(())
            })
            .await?;
        check_ok(reply)
    }

    /// Looks up user and group names by numeric id. The result has one name
    /// per id, in the order asked; an id the server cannot resolve gets an
    /// empty name.
    pub async fn users_groups_by_id(
        &self,
        uids: &[u32],
        gids: &[u32],
    ) -> Result<(Vec<Bytes>, Vec<Bytes>), Error> {
        let pack = |ids: &[u32]| {
            let mut v = BytesMut::with_capacity(ids.len() * 4);
            for id in ids {
                v.put_u32(*id);
            }
            v
        };
        let (uid_bytes, gid_bytes) = (pack(uids), pack(gids));
        let reply = self
            .ext_request(USERS_GROUPS_BY_ID, |out| {
                put_string(out, &uid_bytes)?;
                put_string(out, &gid_bytes)
            })
            .await?;
        let Reply::Extended(data) = reply else {
            return Err(unexpected(reply, "user and group names"));
        };
        let mut r = Reader::new(data);
        let decode = |e| Error::Protocol(ProtocolError::Decode(e));
        let users = strings(r.string().map_err(decode)?)?;
        let groups = strings(r.string().map_err(decode)?)?;
        r.finish().map_err(decode)?;
        if users.len() != uids.len() || groups.len() != gids.len() {
            return Err(Error::Protocol(ProtocolError::UnexpectedReply(
                "one name per id",
            )));
        }
        Ok((users, groups))
    }
}

fn single_name(reply: Reply) -> Result<Bytes, Error> {
    match reply {
        Reply::Name(mut names) if names.len() == 1 => Ok(names.remove(0).name),
        Reply::Name(_) => Err(Error::Protocol(ProtocolError::UnexpectedReply(
            "exactly one name",
        ))),
        other => Err(unexpected(other, "name")),
    }
}
