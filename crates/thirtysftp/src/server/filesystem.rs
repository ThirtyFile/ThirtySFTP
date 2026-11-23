//! The interface a server implements to expose a file system.

use std::fmt;
use std::future::Future;
use std::io;

use bytes::Bytes;

use crate::extensions::StatVfs;
use crate::wire::{Attributes, NameEntry, OpenFlags, StatusCode};

/// A failure to report to the client as a `STATUS` reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsError {
    /// The code sent to the client.
    pub code: StatusCode,
    /// A message for the client. Keep it free of server internals.
    pub message: String,
}

impl FsError {
    /// An error with the given code and message.
    pub fn new(code: StatusCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The file or directory does not exist.
    pub fn no_such_file() -> Self {
        Self::new(StatusCode::NoSuchFile, "no such file")
    }

    /// The operation is not allowed.
    pub fn permission_denied() -> Self {
        Self::new(StatusCode::PermissionDenied, "permission denied")
    }

    /// A failure with no more specific code.
    pub fn failure(message: impl Into<String>) -> Self {
        Self::new(StatusCode::Failure, message)
    }

    /// The server does not implement the operation.
    pub fn unsupported() -> Self {
        Self::new(StatusCode::OpUnsupported, "operation not supported")
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for FsError {}

impl From<io::Error> for FsError {
    /// Maps the error kind to a status code. The message is the generic
    /// description of the kind, not the operating system's text, which can
    /// name local paths.
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => Self::no_such_file(),
            io::ErrorKind::PermissionDenied => Self::permission_denied(),
            io::ErrorKind::Unsupported => Self::unsupported(),
            kind => Self::failure(kind.to_string()),
        }
    }
}

/// A file system a server exposes.
///
/// The protocol loop calls these methods with already-decoded arguments. Paths
/// are byte strings exactly as the client sent them: the protocol does not
/// say they are UTF-8, and the implementation decides what they mean. The
/// open file and directory types are the implementation's own; the loop maps
/// them to opaque handles, so an implementation never sees handle bytes the
/// client chose and a client can only name handles that were issued to it.
///
/// Requests on different handles, and path-based requests, run concurrently,
/// each in its own task. Requests on one handle run one at a time, in the
/// order they arrived. A method that waits a long time stalls only its own
/// request.
///
/// Every method has the same shape: it takes `&self` and returns a `Send`
/// future. Write them as `async fn` in the implementation.
///
/// The extension methods have defaults that refuse. The server announces an
/// extension only if it is listed in [`extensions`](Self::extensions), so
/// override the method and list its name together.
pub trait Filesystem: Send + Sync + 'static {
    /// An open file.
    type File: Send + Sync + 'static;
    /// An open directory.
    type Dir: Send + Sync + 'static;

    /// Opens a file. `attrs` apply if the call creates the file.
    fn open(
        &self,
        path: &[u8],
        flags: OpenFlags,
        attrs: &Attributes,
    ) -> impl Future<Output = Result<Self::File, FsError>> + Send;

    /// Closes a file. The default drops it.
    fn close_file(&self, file: Self::File) -> impl Future<Output = Result<(), FsError>> + Send {
        drop(file);
        async { Ok(()) }
    }

    /// Reads up to `len` bytes at `offset`. Returns `None` at the end of the
    /// file. Fewer bytes than asked for are allowed; more are cut off.
    fn read(
        &self,
        file: &Self::File,
        offset: u64,
        len: u32,
    ) -> impl Future<Output = Result<Option<Bytes>, FsError>> + Send;

    /// Writes `data` at `offset`.
    fn write(
        &self,
        file: &Self::File,
        offset: u64,
        data: Bytes,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Attributes of an open file.
    fn fstat(&self, file: &Self::File) -> impl Future<Output = Result<Attributes, FsError>> + Send;

    /// Changes attributes of an open file.
    fn fsetstat(
        &self,
        file: &Self::File,
        attrs: &Attributes,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Attributes of a path, following symbolic links.
    fn stat(&self, path: &[u8]) -> impl Future<Output = Result<Attributes, FsError>> + Send;

    /// Attributes of a path, not following a symbolic link at the end.
    fn lstat(&self, path: &[u8]) -> impl Future<Output = Result<Attributes, FsError>> + Send;

    /// Changes attributes of a path.
    fn setstat(
        &self,
        path: &[u8],
        attrs: &Attributes,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Opens a directory for listing.
    fn open_dir(&self, path: &[u8]) -> impl Future<Output = Result<Self::Dir, FsError>> + Send;

    /// Closes a directory. The default drops it.
    fn close_dir(&self, dir: Self::Dir) -> impl Future<Output = Result<(), FsError>> + Send {
        drop(dir);
        async { Ok(()) }
    }

    /// The next batch of entries, or `None` when there are no more. A batch
    /// may be any size; the loop hands the client a bounded number at a time
    /// and keeps the rest. `.` and `..` are listed if the implementation
    /// returns them.
    fn read_dir(
        &self,
        dir: &Self::Dir,
    ) -> impl Future<Output = Result<Option<Vec<NameEntry>>, FsError>> + Send;

    /// Removes a file.
    fn remove(&self, path: &[u8]) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Creates a directory.
    fn mkdir(
        &self,
        path: &[u8],
        attrs: &Attributes,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Removes an empty directory.
    fn rmdir(&self, path: &[u8]) -> impl Future<Output = Result<(), FsError>> + Send;

    /// The canonical absolute form of a path. The last name may be missing; the
    /// directories above it must exist.
    fn realpath(&self, path: &[u8]) -> impl Future<Output = Result<Bytes, FsError>> + Send;

    /// Renames a file or directory. Version 3 does not say what happens if
    /// `to` exists; refusing, as OpenSSH does, is the safe choice.
    fn rename(&self, from: &[u8], to: &[u8]) -> impl Future<Output = Result<(), FsError>> + Send;

    /// The target of a symbolic link.
    fn read_link(&self, path: &[u8]) -> impl Future<Output = Result<Bytes, FsError>> + Send;

    /// Creates the symbolic link `link` pointing at `target`.
    fn symlink(
        &self,
        target: &[u8],
        link: &[u8],
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Names of the extensions this implementation handles, from
    /// [`crate::extensions`]. `limits@openssh.com` is always announced and
    /// need not be listed.
    fn extensions(&self) -> &'static [&'static str] {
        &[]
    }

    /// `posix-rename@openssh.com`: rename, replacing `to` if it exists.
    fn posix_rename(
        &self,
        from: &[u8],
        to: &[u8],
    ) -> impl Future<Output = Result<(), FsError>> + Send {
        let _ = (from, to);
        async { Err(FsError::unsupported()) }
    }

    /// `hardlink@openssh.com`: makes `new` another name for `existing`.
    fn hardlink(
        &self,
        existing: &[u8],
        new: &[u8],
    ) -> impl Future<Output = Result<(), FsError>> + Send {
        let _ = (existing, new);
        async { Err(FsError::unsupported()) }
    }

    /// `fsync@openssh.com`: flushes a file to stable storage.
    fn fsync(&self, file: &Self::File) -> impl Future<Output = Result<(), FsError>> + Send {
        let _ = file;
        async { Err(FsError::unsupported()) }
    }

    /// `lsetstat@openssh.com`: like `setstat`, without following a final
    /// symbolic link.
    fn lsetstat(
        &self,
        path: &[u8],
        attrs: &Attributes,
    ) -> impl Future<Output = Result<(), FsError>> + Send {
        let _ = (path, attrs);
        async { Err(FsError::unsupported()) }
    }

    /// `statvfs@openssh.com`: statistics of the file system holding `path`.
    fn statvfs(&self, path: &[u8]) -> impl Future<Output = Result<StatVfs, FsError>> + Send {
        let _ = path;
        async { Err(FsError::unsupported()) }
    }

    /// `fstatvfs@openssh.com`: statistics of the file system holding a file.
    fn fstatvfs(&self, file: &Self::File) -> impl Future<Output = Result<StatVfs, FsError>> + Send {
        let _ = file;
        async { Err(FsError::unsupported()) }
    }

    /// `expand-path@openssh.com`: expands `~`, `~user` and relative paths.
    fn expand_path(&self, path: &[u8]) -> impl Future<Output = Result<Bytes, FsError>> + Send {
        let _ = path;
        async { Err(FsError::unsupported()) }
    }

    /// `home-directory`: the home directory of `user`, where the empty name
    /// means the connected user.
    fn home_directory(&self, user: &[u8]) -> impl Future<Output = Result<Bytes, FsError>> + Send {
        let _ = user;
        async { Err(FsError::unsupported()) }
    }

    /// `users-groups-by-id@openssh.com`: one name per id, empty for an id
    /// that does not resolve.
    fn users_groups_by_id(
        &self,
        uids: &[u32],
        gids: &[u32],
    ) -> impl Future<Output = Result<(Vec<Bytes>, Vec<Bytes>), FsError>> + Send {
        let _ = (uids, gids);
        async { Err(FsError::unsupported()) }
    }

    /// `copy-data`: copies `len` bytes (to the end of the source if 0) from
    /// one open file to another. The default reads and writes through the
    /// methods above; override it to copy without moving the data.
    fn copy_data(
        &self,
        source: &Self::File,
        source_offset: u64,
        len: u64,
        destination: &Self::File,
        destination_offset: u64,
    ) -> impl Future<Output = Result<(), FsError>> + Send {
        async move {
            const STEP: u32 = 64 * 1024;
            let mut done = 0u64;
            while len == 0 || done < len {
                let want = if len == 0 {
                    STEP
                } else {
                    u32::try_from((len - done).min(u64::from(STEP))).unwrap_or(STEP)
                };
                let at = source_offset
                    .checked_add(done)
                    .ok_or_else(|| FsError::failure("offset overflow"))?;
                let Some(data) = self.read(source, at, want).await? else {
                    break;
                };
                if data.is_empty() {
                    break;
                }
                let n = data.len() as u64;
                let to = destination_offset
                    .checked_add(done)
                    .ok_or_else(|| FsError::failure("offset overflow"))?;
                self.write(destination, to, data).await?;
                done += n;
            }
            Ok(())
        }
    }
}
