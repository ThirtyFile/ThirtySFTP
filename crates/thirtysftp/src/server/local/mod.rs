//! A [`Filesystem`] that serves a directory of the local disk, confined to it.
//!
//! [`LocalFs::new`] takes a root directory. Clients see that directory as `/`
//! and cannot name anything outside it: `..` stops at the root, and symbolic
//! links are resolved inside the same namespace, so a link to `/etc` points at
//! the `etc` of the served tree, not of the host. The file system calls run on
//! Tokio's blocking pool.
//!
//! # How the confinement works
//!
//! On Unix a path is resolved one component at a time against open directory
//! descriptors, with `O_NOFOLLOW` on each step. The kernel is never asked to
//! follow a link or to interpret `..`, so there is no moment at which a
//! swapped component can redirect a request out of the tree: the request
//! fails instead. On Windows a path is checked component by component for
//! reparse points, names the platform treats specially are refused, and the
//! directories along the way are held open (without delete sharing) while the
//! request runs; symbolic links and junctions are not followed or created at
//! all.
//!
//! # What it does not protect against
//!
//! - **Other processes changing the tree.** The guarantees hold against a
//!   client of this server. A local user who can rename directories above the
//!   root while it runs can move the tree itself.
//! - **Hard links** made by someone else to files outside the root give
//!   access to that file's content from inside it.
//! - **Mount points** inside the root are crossed like any other directory.
//! - **Absolute link targets** created by clients are stored as they are. A
//!   link to `/etc/passwd` is harmless to this server, but another program
//!   that reads the directory will follow it to the host's file. Do not serve
//!   a directory that other programs treat as trusted input.
//! - **`..` after a missing directory.** Unix resolves one component at a
//!   time, so `/missing/../x` fails; Windows resolves `..` before looking
//!   at the disk, so it names `/x`. Neither can leave the root.
//! - **Special files.** Only regular files and directories can be opened;
//!   pipes, sockets and devices are refused.
//! - Attributes of an existing file that cannot be opened for reading (mode
//!   000) cannot be changed with `setstat` on Unix; times and ownership can.
//!
//! # Attributes by platform
//!
//! On Unix, size, owner, permissions and times are those of the file. On
//! Windows there are no owner ids; permissions are synthesised (`0o755` for
//! directories, `0o644` for files, without the write bits if the read-only
//! attribute is set) and `setstat` maps the owner write bit back to that
//! attribute. Times are whole seconds on both.

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::Semaphore;

use super::filesystem::{Filesystem, FsError};
use crate::extensions as ext;
use crate::extensions::StatVfs;
use crate::wire::{Attributes, NameEntry, OpenFlags};

#[cfg(unix)]
#[path = "unix.rs"]
mod sys;
#[cfg(windows)]
#[path = "windows.rs"]
mod sys;

pub use sys::{LocalDir, LocalFile};

/// Symbolic links followed while resolving one path.
#[cfg(unix)]
const MAX_LINK_HOPS: usize = 40;
/// File system calls one `LocalFs` runs at once unless told otherwise.
const DEFAULT_OPERATIONS: usize = 64;
/// Longest path or link target accepted, in bytes.
const MAX_PATH_LEN: usize = 4096;

/// Splits a client path into components, dropping empty ones and `.`.
/// `..` is kept for the resolver, which treats it as moving up inside the
/// tree.
fn components(path: &[u8]) -> Result<VecDeque<Vec<u8>>, FsError> {
    if path.len() > MAX_PATH_LEN || path.contains(&0) {
        return Err(FsError::failure("invalid path"));
    }
    Ok(path
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .map(<[u8]>::to_vec)
        .collect())
}

/// A directory of the local disk served as a file system. See the
/// [module documentation](self) for what is and is not protected.
pub struct LocalFs {
    root: Arc<sys::Root>,
    read_only: bool,
    account_names: bool,
    special_mode_bits: bool,
    /// Bounds the file system calls in progress on the blocking pool.
    operations: Arc<Semaphore>,
}

impl LocalFs {
    /// Serves the directory at `root`. The path itself may contain symbolic
    /// links; they are resolved once, here.
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            root: Arc::new(sys::Root::open(root.as_ref())?),
            read_only: false,
            account_names: false,
            special_mode_bits: false,
            operations: Arc::new(Semaphore::new(DEFAULT_OPERATIONS)),
        })
    }

    /// Refuses everything that would change the tree: writes, creating,
    /// removing, renaming and attribute changes. Reads and listings work.
    pub fn read_only(mut self, on: bool) -> Self {
        self.read_only = on;
        self
    }

    /// Offers `users-groups-by-id@openssh.com`, which tells clients the names
    /// of the host's users and groups for numeric ids (on Unix, from
    /// `/etc/passwd` and `/etc/group`). Off by default: the names are facts
    /// about the host, not about the served directory. Has no effect on
    /// Windows, where there are no such ids.
    pub fn account_names(mut self, on: bool) -> Self {
        self.account_names = on;
        self
    }

    /// Lets clients set the set-user-id and set-group-id bits. Off by default:
    /// those bits are dropped from every mode a client sends, because a
    /// client that can upload a file and mark it set-user-id has made a
    /// program that runs as the server's user. The sticky bit is always
    /// allowed. Has no effect on Windows.
    pub fn special_mode_bits(mut self, on: bool) -> Self {
        self.special_mode_bits = on;
        self
    }

    /// The most file system calls this file system runs at once (64 by
    /// default; at least 1). They run on Tokio's blocking pool, which the
    /// whole program shares, so a file system that stalls, such as a dead
    /// network mount, takes at most this many of its threads; further
    /// requests wait their turn.
    pub fn max_operations(mut self, count: usize) -> Self {
        self.operations = Arc::new(Semaphore::new(count.clamp(1, Semaphore::MAX_PERMITS)));
        self
    }

    /// Runs a file system call on the blocking pool, within the limit.
    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T, FsError> + Send + 'static,
    ) -> Result<T, FsError> {
        let _permit = self
            .operations
            .acquire()
            .await
            .map_err(|_| FsError::failure("internal error"))?;
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|_| FsError::failure("internal error"))?
    }

    /// The permission bits a client may set.
    fn mode(&self, mode: Option<u32>) -> Option<u32> {
        let allowed = if self.special_mode_bits {
            0o7777
        } else {
            0o1777
        };
        mode.map(|m| m & allowed)
    }

    /// `attrs` with its permissions limited to what a client may set.
    fn attrs(&self, attrs: &Attributes) -> Attributes {
        Attributes {
            permissions: self.mode(attrs.permissions),
            ..attrs.clone()
        }
    }

    fn writable(&self) -> Result<(), FsError> {
        if self.read_only {
            Err(FsError::new(
                crate::wire::StatusCode::PermissionDenied,
                "read-only file system",
            ))
        } else {
            Ok(())
        }
    }
}

impl Filesystem for LocalFs {
    type File = LocalFile;
    type Dir = LocalDir;

    async fn open(
        &self,
        path: &[u8],
        flags: OpenFlags,
        attrs: &Attributes,
    ) -> Result<LocalFile, FsError> {
        let changes = [
            OpenFlags::WRITE,
            OpenFlags::APPEND,
            OpenFlags::CREATE,
            OpenFlags::TRUNCATE,
        ]
        .into_iter()
        .any(|f| flags.contains(f));
        if changes {
            self.writable()?;
        }
        let (root, path, perms) = (
            self.root.clone(),
            path.to_vec(),
            self.mode(attrs.permissions),
        );
        self.blocking(move || root.open_file(&path, flags, perms))
            .await
    }

    async fn read(
        &self,
        file: &LocalFile,
        offset: u64,
        len: u32,
    ) -> Result<Option<Bytes>, FsError> {
        let file = file.clone();
        self.blocking(move || file.read_at(offset, len)).await
    }

    async fn write(&self, file: &LocalFile, offset: u64, data: Bytes) -> Result<(), FsError> {
        self.writable()?;
        let file = file.clone();
        self.blocking(move || file.write_at(offset, &data)).await
    }

    async fn fstat(&self, file: &LocalFile) -> Result<Attributes, FsError> {
        let file = file.clone();
        self.blocking(move || file.fstat()).await
    }

    async fn fsetstat(&self, file: &LocalFile, attrs: &Attributes) -> Result<(), FsError> {
        self.writable()?;
        let (file, attrs) = (file.clone(), self.attrs(attrs));
        self.blocking(move || file.fset(&attrs)).await
    }

    async fn stat(&self, path: &[u8]) -> Result<Attributes, FsError> {
        let (root, path) = (self.root.clone(), path.to_vec());
        self.blocking(move || root.stat(&path, true)).await
    }

    async fn lstat(&self, path: &[u8]) -> Result<Attributes, FsError> {
        let (root, path) = (self.root.clone(), path.to_vec());
        self.blocking(move || root.stat(&path, false)).await
    }

    async fn setstat(&self, path: &[u8], attrs: &Attributes) -> Result<(), FsError> {
        self.writable()?;
        let (root, path, attrs) = (self.root.clone(), path.to_vec(), self.attrs(attrs));
        self.blocking(move || root.set_attrs(&path, true, &attrs))
            .await
    }

    async fn open_dir(&self, path: &[u8]) -> Result<LocalDir, FsError> {
        let (root, path) = (self.root.clone(), path.to_vec());
        self.blocking(move || root.open_dir(&path)).await
    }

    async fn read_dir(&self, dir: &LocalDir) -> Result<Option<Vec<NameEntry>>, FsError> {
        let dir = dir.clone();
        self.blocking(move || dir.read_batch()).await
    }

    async fn remove(&self, path: &[u8]) -> Result<(), FsError> {
        self.writable()?;
        let (root, path) = (self.root.clone(), path.to_vec());
        self.blocking(move || root.remove(&path)).await
    }

    async fn mkdir(&self, path: &[u8], attrs: &Attributes) -> Result<(), FsError> {
        self.writable()?;
        let (root, path, perms) = (
            self.root.clone(),
            path.to_vec(),
            self.mode(attrs.permissions),
        );
        self.blocking(move || root.mkdir(&path, perms)).await
    }

    async fn rmdir(&self, path: &[u8]) -> Result<(), FsError> {
        self.writable()?;
        let (root, path) = (self.root.clone(), path.to_vec());
        self.blocking(move || root.rmdir(&path)).await
    }

    async fn realpath(&self, path: &[u8]) -> Result<Bytes, FsError> {
        let (root, path) = (self.root.clone(), path.to_vec());
        self.blocking(move || root.realpath(&path).map(Bytes::from))
            .await
    }

    async fn rename(&self, from: &[u8], to: &[u8]) -> Result<(), FsError> {
        self.writable()?;
        let (root, from, to) = (self.root.clone(), from.to_vec(), to.to_vec());
        self.blocking(move || root.rename(&from, &to, false)).await
    }

    async fn read_link(&self, path: &[u8]) -> Result<Bytes, FsError> {
        let (root, path) = (self.root.clone(), path.to_vec());
        self.blocking(move || root.read_link(&path).map(Bytes::from))
            .await
    }

    async fn symlink(&self, target: &[u8], link: &[u8]) -> Result<(), FsError> {
        self.writable()?;
        let (root, target, link) = (self.root.clone(), target.to_vec(), link.to_vec());
        self.blocking(move || root.symlink(&target, &link)).await
    }

    fn extensions(&self) -> &'static [&'static str] {
        #[cfg(unix)]
        {
            const BASE: &[&str] = &[
                ext::POSIX_RENAME,
                ext::HARDLINK,
                ext::FSYNC,
                ext::LSETSTAT,
                ext::STATVFS,
                ext::FSTATVFS,
                ext::EXPAND_PATH,
                ext::COPY_DATA,
                ext::HOME_DIRECTORY,
            ];
            const WITH_NAMES: &[&str] = &[
                ext::POSIX_RENAME,
                ext::HARDLINK,
                ext::FSYNC,
                ext::LSETSTAT,
                ext::STATVFS,
                ext::FSTATVFS,
                ext::EXPAND_PATH,
                ext::COPY_DATA,
                ext::HOME_DIRECTORY,
                ext::USERS_GROUPS_BY_ID,
            ];
            if self.account_names { WITH_NAMES } else { BASE }
        }
        #[cfg(not(unix))]
        {
            &[
                ext::POSIX_RENAME,
                ext::HARDLINK,
                ext::FSYNC,
                ext::EXPAND_PATH,
                ext::COPY_DATA,
                ext::HOME_DIRECTORY,
            ]
        }
    }

    async fn posix_rename(&self, from: &[u8], to: &[u8]) -> Result<(), FsError> {
        self.writable()?;
        let (root, from, to) = (self.root.clone(), from.to_vec(), to.to_vec());
        self.blocking(move || root.rename(&from, &to, true)).await
    }

    async fn hardlink(&self, existing: &[u8], new: &[u8]) -> Result<(), FsError> {
        self.writable()?;
        let (root, existing, new) = (self.root.clone(), existing.to_vec(), new.to_vec());
        self.blocking(move || root.hardlink(&existing, &new)).await
    }

    async fn fsync(&self, file: &LocalFile) -> Result<(), FsError> {
        let file = file.clone();
        self.blocking(move || file.sync()).await
    }

    async fn lsetstat(&self, path: &[u8], attrs: &Attributes) -> Result<(), FsError> {
        self.writable()?;
        let (root, path, attrs) = (self.root.clone(), path.to_vec(), self.attrs(attrs));
        self.blocking(move || root.set_attrs(&path, false, &attrs))
            .await
    }

    async fn statvfs(&self, path: &[u8]) -> Result<StatVfs, FsError> {
        let (root, path, read_only) = (self.root.clone(), path.to_vec(), self.read_only);
        let mut v = self.blocking(move || root.statvfs(&path)).await?;
        if read_only {
            v.flags |= StatVfs::READ_ONLY;
        }
        Ok(v)
    }

    async fn fstatvfs(&self, file: &LocalFile) -> Result<StatVfs, FsError> {
        let (file, read_only) = (file.clone(), self.read_only);
        let mut v = self.blocking(move || file.statvfs()).await?;
        if read_only {
            v.flags |= StatVfs::READ_ONLY;
        }
        Ok(v)
    }

    async fn expand_path(&self, path: &[u8]) -> Result<Bytes, FsError> {
        let rest = match path {
            b"~" => &b""[..],
            p if p.starts_with(b"~/") => &p[2..],
            p => p,
        };
        self.realpath(rest).await
    }

    async fn home_directory(&self, _user: &[u8]) -> Result<Bytes, FsError> {
        Ok(Bytes::from_static(b"/"))
    }

    #[cfg(unix)]
    async fn users_groups_by_id(
        &self,
        uids: &[u32],
        gids: &[u32],
    ) -> Result<(Vec<Bytes>, Vec<Bytes>), FsError> {
        if !self.account_names {
            return Err(FsError::unsupported());
        }
        let (uids, gids) = (uids.to_vec(), gids.to_vec());
        self.blocking(move || Ok(sys::users_groups(&uids, &gids)))
            .await
    }
}
