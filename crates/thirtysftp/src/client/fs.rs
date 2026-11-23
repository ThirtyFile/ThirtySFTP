//! Functions in the manner of `tokio::fs`, working on a [`Session`].
//!
//! Paths are byte strings: anything that is `AsRef<[u8]>`, such as `&str`,
//! `String` and `Vec<u8>`. The server's path syntax applies, which is
//! normally `/`-separated.

use super::{Dir, File, Session};
use crate::error::Error;
use crate::extensions;
use crate::wire::{Attributes, StatusCode};

/// Reads a whole file.
pub async fn read(session: &Session, path: impl AsRef<[u8]>) -> Result<Vec<u8>, Error> {
    let file = File::open(session, path).await?;
    let data = file.read_all().await;
    let closed = file.close().await;
    let data = data?;
    closed?;
    Ok(data)
}

/// Writes a whole file, creating it or cutting it to length 0 first.
///
/// The contents are copied once. To send a buffer you already hold as
/// [`bytes::Bytes`] without copying it, use [`File::write_all_at`].
pub async fn write(
    session: &Session,
    path: impl AsRef<[u8]>,
    contents: impl AsRef<[u8]>,
) -> Result<(), Error> {
    let file = File::create(session, path).await?;
    let contents = bytes::Bytes::copy_from_slice(contents.as_ref());
    let written = file.write_all_at(0, contents).await;
    let closed = file.close().await;
    written?;
    closed
}

/// Attributes of `path`, following symbolic links.
pub async fn metadata(session: &Session, path: impl AsRef<[u8]>) -> Result<Attributes, Error> {
    session.stat(path).await
}

/// Attributes of `path`, not following a symbolic link at the end.
pub async fn symlink_metadata(
    session: &Session,
    path: impl AsRef<[u8]>,
) -> Result<Attributes, Error> {
    session.lstat(path).await
}

/// Whether `path` exists. A missing file is `Ok(false)`; any other failure is
/// an error.
pub async fn try_exists(session: &Session, path: impl AsRef<[u8]>) -> Result<bool, Error> {
    match session.stat(path).await {
        Ok(_) => Ok(true),
        Err(e) if e.status_code() == Some(StatusCode::NoSuchFile) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Opens a directory for listing.
pub async fn read_dir(session: &Session, path: impl AsRef<[u8]>) -> Result<Dir, Error> {
    Dir::open(session, path).await
}

/// Creates one directory.
pub async fn create_dir(session: &Session, path: impl AsRef<[u8]>) -> Result<(), Error> {
    session.mkdir(path, Attributes::default()).await
}

/// Creates a directory and any missing parents. A directory that already
/// exists is not an error.
pub async fn create_dir_all(session: &Session, path: impl AsRef<[u8]>) -> Result<(), Error> {
    let path = path.as_ref();
    let mut prefixes: Vec<&[u8]> = path
        .iter()
        .enumerate()
        .filter(|&(i, &b)| b == b'/' && i > 0 && path[i - 1] != b'/')
        .map(|(i, _)| &path[..i])
        .collect();
    if !path.is_empty() && !path.ends_with(b"/") {
        prefixes.push(path);
    }
    for prefix in prefixes {
        if let Err(e) = session.mkdir(prefix, Attributes::default()).await {
            // Already there is fine; anything else is not.
            match session.stat(prefix).await {
                Ok(attrs) if attrs.is_dir() => {}
                _ => return Err(e),
            }
        }
    }
    Ok(())
}

/// Removes a file.
pub async fn remove_file(session: &Session, path: impl AsRef<[u8]>) -> Result<(), Error> {
    session.remove(path).await
}

/// Removes an empty directory.
pub async fn remove_dir(session: &Session, path: impl AsRef<[u8]>) -> Result<(), Error> {
    session.rmdir(path).await
}

/// Renames `from` to `to`, replacing `to` if it exists.
///
/// Uses `posix-rename@openssh.com` when the server has it. Version 3 of the
/// protocol does not say whether a plain rename replaces an existing target,
/// and many servers refuse; without the extension this falls back to the
/// plain request and inherits that behaviour.
pub async fn rename(
    session: &Session,
    from: impl AsRef<[u8]>,
    to: impl AsRef<[u8]>,
) -> Result<(), Error> {
    if session.supports(extensions::POSIX_RENAME) {
        session.posix_rename(from, to).await
    } else {
        session.rename(from, to).await
    }
}

/// The canonical absolute form of `path`.
pub async fn canonicalize(
    session: &Session,
    path: impl AsRef<[u8]>,
) -> Result<bytes::Bytes, Error> {
    session.realpath(path).await
}

/// The target of the symbolic link `path`.
pub async fn read_link(session: &Session, path: impl AsRef<[u8]>) -> Result<bytes::Bytes, Error> {
    session.readlink(path).await
}

/// Creates the symbolic link `link` pointing at `target`.
pub async fn symlink(
    session: &Session,
    target: impl AsRef<[u8]>,
    link: impl AsRef<[u8]>,
) -> Result<(), Error> {
    session.symlink(target, link).await
}

/// Sets the permission bits of `path`.
pub async fn set_permissions(
    session: &Session,
    path: impl AsRef<[u8]>,
    mode: u32,
) -> Result<(), Error> {
    session
        .setstat(
            path,
            Attributes {
                permissions: Some(mode),
                ..Attributes::default()
            },
        )
        .await
}
