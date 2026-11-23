//! Dispatch of extended requests to the file system.

use bytes::{Buf, Bytes, BytesMut};

use super::filesystem::{Filesystem, FsError};
use super::reply::{bad_request, invalid_handle, name_reply, ok_reply};
use super::table::{Ctx, Kind};
use crate::extensions::{self as ext, Limits};
use crate::wire::{Attributes, Reader, Reply};

pub(super) fn file_of<F: Filesystem>(kinds: &[Kind<F>], i: usize) -> Result<&F::File, FsError> {
    match kinds.get(i) {
        Some(Kind::File(file)) => Ok(file),
        _ => Err(invalid_handle()),
    }
}

pub(super) async fn extended<F: Filesystem>(
    ctx: &Ctx<F>,
    name: &[u8],
    data: Bytes,
    kinds: &[Kind<F>],
) -> Result<Reply, FsError> {
    let fs = &*ctx.fs;
    if name == ext::LIMITS.as_bytes() {
        let limits = Limits {
            max_packet_length: u64::from(ctx.cfg.max_packet),
            max_read_length: u64::from(ctx.cfg.max_read_length),
            max_write_length: u64::from(ctx.cfg.max_write_length),
            max_open_handles: ctx.cfg.max_handles as u64,
        };
        return Ok(Reply::Extended(limits.encode()));
    }
    let Some(known) = ctx.announced.iter().find(|n| n.as_bytes() == name) else {
        return Err(FsError::unsupported());
    };
    let mut r = Reader::new(data);
    let bad = |_| bad_request();
    match *known {
        ext::POSIX_RENAME => {
            let (from, to) = (r.string().map_err(bad)?, r.string().map_err(bad)?);
            r.finish().map_err(bad)?;
            fs.posix_rename(&from, &to).await?;
            Ok(ok_reply())
        }
        ext::HARDLINK => {
            let (existing, new) = (r.string().map_err(bad)?, r.string().map_err(bad)?);
            r.finish().map_err(bad)?;
            fs.hardlink(&existing, &new).await?;
            Ok(ok_reply())
        }
        ext::FSYNC => {
            r.string().map_err(bad)?;
            r.finish().map_err(bad)?;
            fs.fsync(file_of(kinds, 0)?).await?;
            Ok(ok_reply())
        }
        ext::FSTATVFS => {
            r.string().map_err(bad)?;
            r.finish().map_err(bad)?;
            let v = fs.fstatvfs(file_of(kinds, 0)?).await?;
            Ok(Reply::Extended(v.encode()))
        }
        ext::STATVFS => {
            let path = r.string().map_err(bad)?;
            r.finish().map_err(bad)?;
            Ok(Reply::Extended(fs.statvfs(&path).await?.encode()))
        }
        ext::LSETSTAT => {
            let path = r.string().map_err(bad)?;
            let attrs = Attributes::decode(&mut r).map_err(bad)?;
            r.finish().map_err(bad)?;
            fs.lsetstat(&path, &attrs).await?;
            Ok(ok_reply())
        }
        ext::EXPAND_PATH => {
            let path = r.string().map_err(bad)?;
            r.finish().map_err(bad)?;
            Ok(name_reply(fs.expand_path(&path).await?))
        }
        ext::HOME_DIRECTORY => {
            let user = r.string().map_err(bad)?;
            r.finish().map_err(bad)?;
            Ok(name_reply(fs.home_directory(&user).await?))
        }
        ext::COPY_DATA => {
            r.string().map_err(bad)?;
            let (so, len) = (r.u64().map_err(bad)?, r.u64().map_err(bad)?);
            r.string().map_err(bad)?;
            let d_off = r.u64().map_err(bad)?;
            r.finish().map_err(bad)?;
            let (src, dst) = (file_of(kinds, 0)?, file_of(kinds, 1)?);
            fs.copy_data(src, so, len, dst, d_off).await?;
            Ok(ok_reply())
        }
        ext::USERS_GROUPS_BY_ID => {
            let (uids, gids) = (r.string().map_err(bad)?, r.string().map_err(bad)?);
            r.finish().map_err(bad)?;
            let unpack = |b: &Bytes| -> Result<Vec<u32>, FsError> {
                if b.len() & 3 != 0 {
                    return Err(bad_request());
                }
                let mut b = b.clone();
                let mut out = Vec::with_capacity(b.len() / 4);
                while b.has_remaining() {
                    out.push(b.get_u32());
                }
                Ok(out)
            };
            let (uids, gids) = (unpack(&uids)?, unpack(&gids)?);
            let (users, groups) = fs.users_groups_by_id(&uids, &gids).await?;
            let pack = |names: &[Bytes]| -> Result<Bytes, FsError> {
                let mut out = BytesMut::new();
                for n in names {
                    crate::wire::put_string(&mut out, n)
                        .map_err(|_| FsError::failure("name too long"))?;
                }
                Ok(out.freeze())
            };
            let (users, groups) = (pack(&users)?, pack(&groups)?);
            let mut reply = BytesMut::new();
            crate::wire::put_string(&mut reply, &users)
                .and_then(|()| crate::wire::put_string(&mut reply, &groups))
                .map_err(|_| FsError::failure("reply too large"))?;
            Ok(Reply::Extended(reply.freeze()))
        }
        _ => Err(FsError::unsupported()),
    }
}

/// Extension names the loop can dispatch.
pub(super) const KNOWN: &[&str] = &[
    ext::POSIX_RENAME,
    ext::STATVFS,
    ext::FSTATVFS,
    ext::HARDLINK,
    ext::FSYNC,
    ext::LSETSTAT,
    ext::LIMITS,
    ext::EXPAND_PATH,
    ext::COPY_DATA,
    ext::HOME_DIRECTORY,
    ext::USERS_GROUPS_BY_ID,
];
