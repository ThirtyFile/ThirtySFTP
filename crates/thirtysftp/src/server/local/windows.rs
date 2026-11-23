//! `LocalFs` on Windows.
//!
//! Windows has no way to open a file relative to a directory handle that the
//! standard library exposes, so this implementation is deliberately narrower
//! than the Unix one. It refuses what it cannot make safe:
//!
//! - Reparse points (symbolic links, junctions, mount points) are never
//!   followed, created or read. A path through one fails; `lstat` reports it as
//!   a symbolic link.
//! - Names the platform treats specially are refused: reserved device names
//!   (`CON`, `NUL`, `COM1`, ...), alternate data streams and drive prefixes
//!   (`:`), wildcards, backslashes, and names ending in a dot or a space.
//! - The root is resolved to a verbatim (`\\?\`) path, which turns off the
//!   Win32 name rewriting that would otherwise make `..` or `/` mean more
//!   than they say.
//!
//! While a request runs, every directory above its target is held open
//! without delete sharing, so none of them can be renamed or replaced by
//! another process in the meantime. The target itself cannot be held (a
//! rename or remove would block on our own handle), so it is checked before
//! the operation and, for opens, again on the handle that was opened.
//!
//! # A known gap
//!
//! A held directory can still be turned into a junction in place by another
//! local process, because that needs no access the open handles can deny. If
//! that happens between the check and the operation, the operation follows
//! the junction. Every operation checks the held directories again when it
//! is done and fails if one has become a junction, which catches the swap in
//! most cases and keeps the result from the client, but it cannot undo a
//! write, and a swap that is undone again in time is not seen. Closing this
//! needs operations relative to directory handles, which the standard library
//! does not offer. So: on Windows, do not serve a tree in which other local
//! users can create junctions. SFTP clients cannot create them through this
//! server.

use std::collections::VecDeque;
use std::fs::{self, File, FileTimes, Metadata, OpenOptions as StdOpen};
use std::io::{self, Write};
use std::os::windows::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use super::components;
use crate::extensions::StatVfs;
use crate::server::filesystem::FsError;
use crate::wire::{Attributes, NameEntry, OpenFlags, Times};

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
const FILE_SHARE_READ_WRITE: u32 = 0x1 | 0x2;
const DIR_BATCH: usize = 256;

fn io_err(e: io::Error) -> FsError {
    match e.kind() {
        io::ErrorKind::AlreadyExists => FsError::failure("file exists"),
        io::ErrorKind::NotADirectory => FsError::failure("not a directory"),
        io::ErrorKind::IsADirectory => FsError::failure("is a directory"),
        io::ErrorKind::DirectoryNotEmpty => FsError::failure("directory not empty"),
        _ => FsError::from(e),
    }
}

fn unsupported_links() -> FsError {
    FsError::failure("symbolic links are not supported on this server")
}

/// Whether `name` is safe to use as one path component.
fn valid_name(name: &str) -> bool {
    const FORBIDDEN: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];
    if name.is_empty()
        || name.contains(FORBIDDEN)
        || name.chars().any(|c| c.is_control())
        || name.ends_with('.')
        || name.ends_with(' ')
    {
        return false;
    }
    // Device names are reserved with or without an extension.
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|p| {
        stem.strip_prefix(p)
            .is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
    });
    !reserved
}

/// Resolves `..` lexically (nothing is a link here, so this is exact) and
/// checks every name.
fn names(path: &[u8]) -> Result<Vec<String>, FsError> {
    let mut out: Vec<String> = Vec::new();
    let queue: VecDeque<Vec<u8>> = components(path)?;
    for part in queue {
        if part == b".." {
            out.pop();
            continue;
        }
        let name = std::str::from_utf8(&part).map_err(|_| FsError::failure("invalid path"))?;
        if !valid_name(name) {
            return Err(FsError::failure("invalid path"));
        }
        out.push(name.to_owned());
    }
    Ok(out)
}

fn is_reparse(md: &Metadata) -> bool {
    md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Opens a directory or file to look at it without following a link, with
/// sharing that stops others from renaming or deleting it. It does not stop
/// anyone from turning a directory into a junction, which needs no access
/// that sharing can refuse; see `Resolved::recheck`.
fn hold(path: &Path) -> io::Result<File> {
    StdOpen::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// A resolved path. The guards keep the directories above it in place.
struct Resolved {
    path: PathBuf,
    names: Vec<String>,
    guards: Vec<File>,
}

impl Resolved {
    fn virtual_path(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for n in &self.names {
            out.push(b'/');
            out.extend_from_slice(n.as_bytes());
        }
        if out.is_empty() {
            out.push(b'/');
        }
        out
    }

    /// Checks the held directories again: none may have become a reparse
    /// point since it was checked. Called when an operation is done, before
    /// its result is given to the client.
    fn recheck(&self) -> Result<(), FsError> {
        for guard in &self.guards {
            if is_reparse(&guard.metadata().map_err(io_err)?) {
                return Err(unsupported_links());
            }
        }
        Ok(())
    }

    /// The root has no parent entry to create, remove or rename.
    fn require_leaf(&self) -> Result<(), FsError> {
        if self.names.is_empty() {
            Err(FsError::failure("operation not allowed on this path"))
        } else {
            Ok(())
        }
    }
}

fn secs(t: io::Result<SystemTime>) -> u32 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| u32::try_from(d.as_secs()).unwrap_or(u32::MAX))
}

fn attrs_of(md: &Metadata) -> Attributes {
    let readonly = md.file_attributes() & FILE_ATTRIBUTE_READONLY != 0;
    let (kind, mode, size) = if is_reparse(md) {
        (0o120_000, 0o777, None)
    } else if md.is_dir() {
        (0o040_000, if readonly { 0o555 } else { 0o755 }, None)
    } else {
        (
            0o100_000,
            if readonly { 0o444 } else { 0o644 },
            Some(md.len()),
        )
    };
    Attributes {
        size,
        owner: None,
        permissions: Some(kind | mode),
        times: Some(Times {
            accessed: secs(md.accessed()),
            modified: secs(md.modified()),
        }),
        extended: Vec::new(),
    }
}

fn system_time(secs: u32) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(u64::from(secs))
}

/// Applies permissions (the read-only attribute) and times to an open handle.
fn apply_to_handle(file: &File, attrs: &Attributes) -> Result<(), FsError> {
    if let Some(mode) = attrs.permissions {
        let mut perms = file.metadata().map_err(io_err)?.permissions();
        perms.set_readonly(mode & 0o200 == 0);
        file.set_permissions(perms).map_err(io_err)?;
    }
    if let Some(t) = attrs.times {
        let times = FileTimes::new()
            .set_accessed(system_time(t.accessed))
            .set_modified(system_time(t.modified));
        file.set_times(times).map_err(io_err)?;
    }
    Ok(())
}

/// The confined directory tree.
pub(super) struct Root {
    path: PathBuf,
}

impl Root {
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let path = fs::canonicalize(path)?;
        if !fs::metadata(&path)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "root is not a directory",
            ));
        }
        Ok(Self { path })
    }

    /// Resolves `path` below the root. Every directory above the last name
    /// must exist and be an ordinary directory; the last name may be missing.
    fn resolve(&self, path: &[u8]) -> Result<Resolved, FsError> {
        let names = names(path)?;
        let mut current = self.path.clone();
        let mut guards = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let next = current.join(name);
            let last = i + 1 == names.len();
            if !last {
                let handle = hold(&next).map_err(io_err)?;
                let md = handle.metadata().map_err(io_err)?;
                if is_reparse(&md) {
                    return Err(unsupported_links());
                }
                if !md.is_dir() {
                    return Err(FsError::failure("not a directory"));
                }
                guards.push(handle);
            }
            current = next;
        }
        Ok(Resolved {
            path: current,
            names,
            guards,
        })
    }

    /// Metadata of the resolved target without following a link.
    fn lstat_of(r: &Resolved) -> Result<Metadata, FsError> {
        fs::symlink_metadata(&r.path).map_err(io_err)
    }

    /// Like `lstat_of`, but a link is an error.
    fn checked(r: &Resolved) -> Result<Metadata, FsError> {
        let md = Self::lstat_of(r)?;
        if is_reparse(&md) {
            return Err(unsupported_links());
        }
        Ok(md)
    }

    pub(super) fn open_file(
        &self,
        path: &[u8],
        flags: OpenFlags,
        _permissions: Option<u32>,
    ) -> Result<LocalFile, FsError> {
        let r = self.resolve(path)?;
        r.require_leaf()?;
        let read = flags.contains(OpenFlags::READ);
        let append = flags.contains(OpenFlags::APPEND);
        let write = flags.contains(OpenFlags::WRITE) || append;
        let create = flags.contains(OpenFlags::CREATE);
        if create && !write {
            return Err(FsError::failure("create needs write access"));
        }
        if let Ok(md) = Self::lstat_of(&r) {
            if is_reparse(&md) {
                return Err(unsupported_links());
            }
            if md.is_dir() {
                return Err(FsError::failure("is a directory"));
            }
        }
        let mut options = StdOpen::new();
        options
            .read(read)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        if append {
            options.append(true);
        } else {
            options.write(write);
        }
        if create {
            if flags.contains(OpenFlags::EXCLUSIVE) {
                options.create_new(true);
            } else {
                options.create(true);
            }
        }
        if write && flags.contains(OpenFlags::TRUNCATE) && !append {
            options.truncate(true);
        }
        let file = options.open(&r.path).map_err(io_err)?;
        // The handle, not the name, decides what was opened.
        let md = file.metadata().map_err(io_err)?;
        if is_reparse(&md) || !md.is_file() {
            return Err(FsError::failure("not a regular file"));
        }
        r.recheck()?;
        Ok(LocalFile {
            file: Arc::new(file),
            read,
            write,
            append,
        })
    }

    pub(super) fn stat(&self, path: &[u8], follow: bool) -> Result<Attributes, FsError> {
        let r = self.resolve(path)?;
        let md = if follow {
            Self::checked(&r)?
        } else {
            Self::lstat_of(&r)?
        };
        r.recheck()?;
        Ok(attrs_of(&md))
    }

    pub(super) fn set_attrs(
        &self,
        path: &[u8],
        _follow: bool,
        attrs: &Attributes,
    ) -> Result<(), FsError> {
        let r = self.resolve(path)?;
        let md = Self::checked(&r)?;
        if let Some(size) = attrs.size {
            if !md.is_file() {
                return Err(FsError::failure("not a regular file"));
            }
            let file = StdOpen::new()
                .write(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&r.path)
                .map_err(io_err)?;
            file.set_len(size).map_err(io_err)?;
        }
        if attrs.permissions.is_some() || attrs.times.is_some() {
            let handle = StdOpen::new()
                .access_mode(FILE_WRITE_ATTRIBUTES | FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&r.path)
                .map_err(io_err)?;
            if is_reparse(&handle.metadata().map_err(io_err)?) {
                return Err(unsupported_links());
            }
            apply_to_handle(&handle, attrs)?;
        }
        r.recheck()
    }

    pub(super) fn open_dir(&self, path: &[u8]) -> Result<LocalDir, FsError> {
        let r = self.resolve(path)?;
        let guard = hold(&r.path).map_err(io_err)?;
        let md = guard.metadata().map_err(io_err)?;
        if is_reparse(&md) {
            return Err(unsupported_links());
        }
        if !md.is_dir() {
            return Err(FsError::failure("not a directory"));
        }
        // The guard is held only while the listing is opened, so that what is
        // listed is the directory that was checked. Holding it for as long as
        // the client keeps the listing open would stop everyone else from
        // renaming or removing the directory meanwhile.
        let entries = fs::read_dir(&r.path).map_err(io_err)?;
        drop(guard);
        r.recheck()?;
        Ok(LocalDir {
            entries: Arc::new(Mutex::new(entries)),
        })
    }

    pub(super) fn remove(&self, path: &[u8]) -> Result<(), FsError> {
        let r = self.resolve(path)?;
        r.require_leaf()?;
        let md = Self::lstat_of(&r)?;
        if md.is_dir() && !is_reparse(&md) {
            return Err(FsError::failure("is a directory"));
        }
        fs::remove_file(&r.path).map_err(io_err)?;
        r.recheck()
    }

    pub(super) fn mkdir(&self, path: &[u8], _permissions: Option<u32>) -> Result<(), FsError> {
        let r = self.resolve(path)?;
        r.require_leaf()?;
        fs::create_dir(&r.path).map_err(io_err)?;
        r.recheck()
    }

    pub(super) fn rmdir(&self, path: &[u8]) -> Result<(), FsError> {
        let r = self.resolve(path)?;
        r.require_leaf()?;
        let md = Self::lstat_of(&r)?;
        if is_reparse(&md) || !md.is_dir() {
            return Err(FsError::failure("not a directory"));
        }
        fs::remove_dir(&r.path).map_err(io_err)?;
        r.recheck()
    }

    pub(super) fn realpath(&self, path: &[u8]) -> Result<Vec<u8>, FsError> {
        Ok(self.resolve(path)?.virtual_path())
    }

    pub(super) fn rename(&self, from: &[u8], to: &[u8], replace: bool) -> Result<(), FsError> {
        let (src, dst) = (self.resolve(from)?, self.resolve(to)?);
        src.require_leaf()?;
        dst.require_leaf()?;
        Self::checked(&src)?;
        if let Ok(md) = Self::lstat_of(&dst) {
            if is_reparse(&md) {
                return Err(unsupported_links());
            }
            if !replace || md.is_dir() {
                return Err(FsError::failure("file exists"));
            }
        }
        fs::rename(&src.path, &dst.path).map_err(io_err)?;
        src.recheck()?;
        dst.recheck()
    }

    pub(super) fn read_link(&self, _path: &[u8]) -> Result<Vec<u8>, FsError> {
        Err(FsError::unsupported())
    }

    pub(super) fn symlink(&self, _target: &[u8], _link: &[u8]) -> Result<(), FsError> {
        Err(FsError::unsupported())
    }

    pub(super) fn hardlink(&self, existing: &[u8], new: &[u8]) -> Result<(), FsError> {
        let (src, dst) = (self.resolve(existing)?, self.resolve(new)?);
        src.require_leaf()?;
        dst.require_leaf()?;
        if !Self::checked(&src)?.is_file() {
            return Err(FsError::failure("not a regular file"));
        }
        fs::hard_link(&src.path, &dst.path).map_err(io_err)?;
        src.recheck()?;
        dst.recheck()
    }

    pub(super) fn statvfs(&self, _path: &[u8]) -> Result<StatVfs, FsError> {
        Err(FsError::unsupported())
    }
}

/// An open file.
#[derive(Clone)]
pub struct LocalFile {
    file: Arc<File>,
    read: bool,
    write: bool,
    append: bool,
}

impl LocalFile {
    pub(super) fn read_at(&self, offset: u64, len: u32) -> Result<Option<Bytes>, FsError> {
        if !self.read {
            return Err(FsError::permission_denied());
        }
        let mut buf = vec![0; len as usize];
        let n = self.file.seek_read(&mut buf, offset).map_err(io_err)?;
        if n == 0 {
            return Ok(None);
        }
        buf.truncate(n);
        Ok(Some(buf.into()))
    }

    pub(super) fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), FsError> {
        if !self.write {
            return Err(FsError::permission_denied());
        }
        if self.append {
            return (&*self.file).write_all(data).map_err(io_err);
        }
        let mut done = 0;
        while done < data.len() {
            let n = self
                .file
                .seek_write(&data[done..], offset + done as u64)
                .map_err(io_err)?;
            if n == 0 {
                return Err(FsError::failure("write failed"));
            }
            done += n;
        }
        Ok(())
    }

    pub(super) fn fstat(&self) -> Result<Attributes, FsError> {
        Ok(attrs_of(&self.file.metadata().map_err(io_err)?))
    }

    pub(super) fn fset(&self, attrs: &Attributes) -> Result<(), FsError> {
        if let Some(size) = attrs.size {
            if !self.write {
                return Err(FsError::permission_denied());
            }
            self.file.set_len(size).map_err(io_err)?;
        }
        apply_to_handle(&self.file, attrs)
    }

    pub(super) fn sync(&self) -> Result<(), FsError> {
        // Windows refuses to flush a handle that cannot write; a handle that
        // cannot write has nothing of its own to flush.
        if !self.write {
            return Ok(());
        }
        self.file.sync_all().map_err(io_err)
    }

    pub(super) fn statvfs(&self) -> Result<StatVfs, FsError> {
        Err(FsError::unsupported())
    }
}

/// An open directory.
#[derive(Clone)]
pub struct LocalDir {
    entries: Arc<Mutex<fs::ReadDir>>,
}

impl LocalDir {
    pub(super) fn read_batch(&self) -> Result<Option<Vec<NameEntry>>, FsError> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = Vec::new();
        while out.len() < DIR_BATCH {
            let Some(entry) = entries.next() else { break };
            let entry = entry.map_err(io_err)?;
            // A name that is not valid UTF-8 cannot be addressed by a client.
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(md) = entry.metadata() else { continue };
            out.push(NameEntry {
                name: Bytes::from(name.into_bytes()),
                long_name: Bytes::new(),
                attrs: attrs_of(&md),
            });
        }
        Ok(if out.is_empty() { None } else { Some(out) })
    }
}
