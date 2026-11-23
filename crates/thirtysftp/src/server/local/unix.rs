//! `LocalFs` on Unix, built on directory file descriptors.
//!
//! No path is ever handed to the operating system whole. A path is resolved
//! one component at a time against open directory descriptors, with
//! `O_NOFOLLOW` on every step, so a symbolic link can never be followed by
//! the kernel behind our back. Links are followed by this code instead, in
//! the virtual namespace of the root: `..` stops at the root, and a link with
//! an absolute target is read as starting from the root, as in a `chroot`.
//! Because the kernel never sees a link target, a link cannot lead out of
//! the tree, and swapping a path component for a link while a request runs
//! makes the request fail instead of escaping.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use rustix::fs::{
    AtFlags, Dir, FileType, Gid, Mode, OFlags, Stat, Timespec, Timestamps, Uid, chownat, fchmod,
    fchown, fstat, fstatvfs, ftruncate, futimens, linkat, mkdirat, openat, readlinkat, renameat,
    statat, symlinkat, unlinkat, utimensat,
};
use rustix::io::Errno;

use super::{MAX_LINK_HOPS, MAX_PATH_LEN, components};
use crate::extensions::StatVfs;
use crate::server::filesystem::FsError;
use crate::wire::{Attributes, NameEntry, OpenFlags, Owner, Times};

/// Entries returned by one `read_dir` call.
const DIR_BATCH: usize = 256;

fn fs_err(e: Errno) -> FsError {
    if e == Errno::NOENT {
        FsError::no_such_file()
    } else if e == Errno::ACCESS || e == Errno::PERM {
        FsError::permission_denied()
    } else if e == Errno::ROFS {
        FsError::new(
            crate::wire::StatusCode::PermissionDenied,
            "read-only file system",
        )
    } else if e == Errno::EXIST {
        FsError::failure("file exists")
    } else if e == Errno::NOTDIR {
        FsError::failure("not a directory")
    } else if e == Errno::ISDIR {
        FsError::failure("is a directory")
    } else if e == Errno::NOTEMPTY {
        FsError::failure("directory not empty")
    } else if e == Errno::LOOP {
        FsError::failure("too many symbolic links")
    } else if e == Errno::NOSPC {
        FsError::failure("no space left on device")
    } else if e == Errno::DQUOT {
        FsError::failure("quota exceeded")
    } else if e == Errno::NAMETOOLONG {
        FsError::failure("name too long")
    } else {
        FsError::failure("operation failed")
    }
}

fn secs(t: i64) -> u32 {
    u32::try_from(t.max(0)).unwrap_or(u32::MAX)
}

// The integer types of `Stat` differ between platforms (`st_mode` is 16 bits
// on macOS, for one), so the casts are needed on some and no-ops on others.
#[allow(clippy::unnecessary_cast)]
fn attrs_of(st: &Stat) -> Attributes {
    Attributes {
        size: u64::try_from(st.st_size).ok(),
        owner: Some(Owner {
            uid: st.st_uid as u32,
            gid: st.st_gid as u32,
        }),
        permissions: Some(st.st_mode as u32),
        times: Some(Times {
            accessed: secs(st.st_atime as i64),
            modified: secs(st.st_mtime as i64),
        }),
        extended: Vec::new(),
    }
}

fn kind(st: &Stat) -> FileType {
    FileType::from_raw_mode(st.st_mode as _)
}

fn mode_bits(mode: u32) -> Mode {
    Mode::from_bits_truncate((mode & 0o7777) as _)
}

fn timestamps(t: Times) -> Timestamps {
    let at = |s: u32| Timespec {
        tv_sec: i64::from(s) as _,
        tv_nsec: 0,
    };
    Timestamps {
        last_access: at(t.accessed),
        last_modification: at(t.modified),
    }
}

fn ids(owner: Owner) -> Result<(Uid, Gid), FsError> {
    // `u32::MAX` means "leave unchanged" to the kernel; it is not an id.
    if owner.uid == u32::MAX || owner.gid == u32::MAX {
        return Err(FsError::failure("invalid owner"));
    }
    Ok((Uid::from_raw(owner.uid), Gid::from_raw(owner.gid)))
}

/// Applies permissions, owner and times to an open file or directory.
fn apply_to_fd(fd: BorrowedFd<'_>, attrs: &Attributes) -> Result<(), FsError> {
    if let Some(mode) = attrs.permissions {
        fchmod(fd, mode_bits(mode)).map_err(fs_err)?;
    }
    if let Some(owner) = attrs.owner {
        let (uid, gid) = ids(owner)?;
        fchown(fd, Some(uid), Some(gid)).map_err(fs_err)?;
    }
    if let Some(times) = attrs.times {
        futimens(fd, &timestamps(times)).map_err(fs_err)?;
    }
    Ok(())
}

/// Where a path ended up: the directory it is in, and the last name.
struct Located {
    /// The directory the path resolved into.
    current: OwnedFd,
    /// The directories above `current`, nearest last, each with the name
    /// `current`'s branch was entered by. The root is at the bottom.
    trail: Vec<(OwnedFd, Vec<u8>)>,
    /// The final name in `current`, `None` if the path resolved to the
    /// directory itself.
    name: Option<Vec<u8>>,
}

impl Located {
    /// Directory and name for use with the `*at` calls. The directory itself
    /// is named `.`.
    fn target(&self) -> (BorrowedFd<'_>, &[u8]) {
        (self.current.as_fd(), self.name.as_deref().unwrap_or(b"."))
    }

    /// The name of an entry that can be created, removed or renamed. The
    /// root, and a directory reached through `..`, have none.
    fn leaf(&self) -> Result<(BorrowedFd<'_>, &[u8]), FsError> {
        match &self.name {
            Some(name) => Ok((self.current.as_fd(), name)),
            None => Err(FsError::failure("operation not allowed on this path")),
        }
    }

    /// The path in the virtual namespace.
    fn virtual_path(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (_, name) in &self.trail {
            out.push(b'/');
            out.extend_from_slice(name);
        }
        if let Some(name) = &self.name {
            out.push(b'/');
            out.extend_from_slice(name);
        }
        if out.is_empty() {
            out.push(b'/');
        }
        out
    }
}

/// The confined directory tree.
pub(super) struct Root {
    fd: OwnedFd,
}

impl Root {
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let fd = openat(
            rustix::fs::CWD,
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self { fd })
    }

    /// Resolves `path` inside the root. With `follow_final` a symbolic link
    /// as the last component is followed too. The last component need not
    /// exist.
    fn walk(&self, path: &[u8], follow_final: bool) -> Result<Located, FsError> {
        let mut queue: VecDeque<Vec<u8>> = components(path)?;
        let mut current = self
            .fd
            .try_clone()
            .map_err(|_| FsError::failure("operation failed"))?;
        let mut trail: Vec<(OwnedFd, Vec<u8>)> = Vec::new();
        let mut hops = 0;
        while let Some(name) = queue.pop_front() {
            if name == b".." {
                if let Some((parent, _)) = trail.pop() {
                    current = parent;
                }
                continue;
            }
            let last = queue.is_empty();
            if last && !follow_final {
                return Ok(Located {
                    current,
                    trail,
                    name: Some(name),
                });
            }
            match statat(&current, name.as_slice(), AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => match kind(&st) {
                    FileType::Symlink => {
                        hops += 1;
                        if hops > MAX_LINK_HOPS {
                            return Err(FsError::failure("too many symbolic links"));
                        }
                        let target = readlinkat(&current, name.as_slice(), Vec::new())
                            .map_err(fs_err)?
                            .into_bytes();
                        if target.len() > MAX_PATH_LEN {
                            return Err(FsError::failure("symbolic link target too long"));
                        }
                        if target.first() == Some(&b'/') {
                            // An absolute target starts at the root.
                            while let Some((parent, _)) = trail.pop() {
                                current = parent;
                            }
                        }
                        for part in components(&target)?.into_iter().rev() {
                            queue.push_front(part);
                        }
                        if queue.len() > MAX_PATH_LEN {
                            return Err(FsError::failure("path too long"));
                        }
                    }
                    FileType::Directory if !last => {
                        let child = openat(
                            &current,
                            name.as_slice(),
                            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::empty(),
                        )
                        .map_err(fs_err)?;
                        trail.push((std::mem::replace(&mut current, child), name));
                    }
                    _ if last => {
                        return Ok(Located {
                            current,
                            trail,
                            name: Some(name),
                        });
                    }
                    _ => return Err(FsError::failure("not a directory")),
                },
                Err(e) if e == Errno::NOENT && last => {
                    return Ok(Located {
                        current,
                        trail,
                        name: Some(name),
                    });
                }
                Err(e) => return Err(fs_err(e)),
            }
        }
        Ok(Located {
            current,
            trail,
            name: None,
        })
    }

    pub(super) fn open_file(
        &self,
        path: &[u8],
        flags: OpenFlags,
        permissions: Option<u32>,
    ) -> Result<LocalFile, FsError> {
        let loc = self.walk(path, true)?;
        let (dir, name) = loc.leaf()?;
        let read = flags.contains(OpenFlags::READ);
        let append = flags.contains(OpenFlags::APPEND);
        let write = flags.contains(OpenFlags::WRITE) || append;
        let mut oflags = OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        oflags |= match (read, write) {
            (true, true) => OFlags::RDWR,
            (false, true) => OFlags::WRONLY,
            _ => OFlags::RDONLY,
        };
        if append {
            oflags |= OFlags::APPEND;
        }
        if flags.contains(OpenFlags::CREATE) {
            oflags |= OFlags::CREATE;
            if flags.contains(OpenFlags::EXCLUSIVE) {
                oflags |= OFlags::EXCL;
            }
        }
        // Opening some special files has effects of its own (a tape rewinds, a
        // pipe's other end wakes), so what is there is looked at first. It
        // can still change before the open, which is why the descriptor is
        // checked again below.
        match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if kind(&st) != FileType::RegularFile => {
                return Err(FsError::failure("not a regular file"));
            }
            Ok(_) => {}
            Err(e) if e == Errno::NOENT => {}
            Err(e) => return Err(fs_err(e)),
        }
        // Truncation waits until the file is known to be a regular file, so
        // that opening a device or pipe cannot destroy anything.
        let fd =
            openat(dir, name, oflags, mode_bits(permissions.unwrap_or(0o644))).map_err(fs_err)?;
        let st = fstat(&fd).map_err(fs_err)?;
        if kind(&st) != FileType::RegularFile {
            return Err(FsError::failure("not a regular file"));
        }
        if write && flags.contains(OpenFlags::TRUNCATE) {
            ftruncate(&fd, 0).map_err(fs_err)?;
        }
        Ok(LocalFile {
            file: Arc::new(File::from(fd)),
            read,
            write,
            append,
        })
    }

    pub(super) fn stat(&self, path: &[u8], follow: bool) -> Result<Attributes, FsError> {
        let loc = self.walk(path, follow)?;
        let (dir, name) = loc.target();
        Ok(attrs_of(
            &statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(fs_err)?,
        ))
    }

    pub(super) fn set_attrs(
        &self,
        path: &[u8],
        follow: bool,
        attrs: &Attributes,
    ) -> Result<(), FsError> {
        let loc = self.walk(path, follow)?;
        let (dir, name) = loc.target();
        let st = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(fs_err)?;
        let file_kind = kind(&st);
        if file_kind == FileType::Symlink && (attrs.permissions.is_some() || attrs.size.is_some()) {
            return Err(FsError::unsupported());
        }
        if !matches!(
            file_kind,
            FileType::RegularFile | FileType::Directory | FileType::Symlink
        ) {
            return Err(FsError::failure("not a regular file or directory"));
        }
        if let Some(size) = attrs.size {
            if file_kind != FileType::RegularFile {
                return Err(FsError::failure("not a regular file"));
            }
            let fd = openat(
                dir,
                name,
                OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(fs_err)?;
            ftruncate(&fd, size).map_err(fs_err)?;
        }
        // Owner and times can be set on the entry itself without opening it.
        // Permissions cannot: `chmod` follows links on Linux, so it goes
        // through a descriptor that was opened without following one.
        if let Some(owner) = attrs.owner {
            let (uid, gid) = ids(owner)?;
            chownat(dir, name, Some(uid), Some(gid), AtFlags::SYMLINK_NOFOLLOW).map_err(fs_err)?;
        }
        if let Some(times) = attrs.times {
            utimensat(dir, name, &timestamps(times), AtFlags::SYMLINK_NOFOLLOW).map_err(fs_err)?;
        }
        if let Some(mode) = attrs.permissions {
            let fd = openat(
                dir,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(fs_err)?;
            fchmod(&fd, mode_bits(mode)).map_err(fs_err)?;
        }
        Ok(())
    }

    pub(super) fn open_dir(&self, path: &[u8]) -> Result<LocalDir, FsError> {
        let loc = self.walk(path, true)?;
        let (dir, name) = loc.target();
        let fd = openat(
            dir,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(fs_err)?;
        Ok(LocalDir {
            dir: Arc::new(Mutex::new(Dir::new(fd).map_err(fs_err)?)),
        })
    }

    pub(super) fn remove(&self, path: &[u8]) -> Result<(), FsError> {
        let loc = self.walk(path, false)?;
        let (dir, name) = loc.leaf()?;
        let st = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(fs_err)?;
        if kind(&st) == FileType::Directory {
            return Err(FsError::failure("is a directory"));
        }
        unlinkat(dir, name, AtFlags::empty()).map_err(fs_err)
    }

    pub(super) fn mkdir(&self, path: &[u8], permissions: Option<u32>) -> Result<(), FsError> {
        let loc = self.walk(path, false)?;
        let (dir, name) = loc.leaf()?;
        mkdirat(dir, name, mode_bits(permissions.unwrap_or(0o755))).map_err(fs_err)
    }

    pub(super) fn rmdir(&self, path: &[u8]) -> Result<(), FsError> {
        let loc = self.walk(path, false)?;
        let (dir, name) = loc.leaf()?;
        unlinkat(dir, name, AtFlags::REMOVEDIR).map_err(fs_err)
    }

    pub(super) fn realpath(&self, path: &[u8]) -> Result<Vec<u8>, FsError> {
        Ok(self.walk(path, true)?.virtual_path())
    }

    pub(super) fn rename(&self, from: &[u8], to: &[u8], replace: bool) -> Result<(), FsError> {
        let (src, dst) = (self.walk(from, false)?, self.walk(to, false)?);
        let ((src_dir, src_name), (dst_dir, dst_name)) = (src.leaf()?, dst.leaf()?);
        statat(src_dir, src_name, AtFlags::SYMLINK_NOFOLLOW).map_err(fs_err)?;
        if !replace {
            // Where the kernel can refuse to replace in the same step as the
            // rename, let it: checking first leaves a gap in which the name
            // can appear.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            match rustix::fs::renameat_with(
                src_dir,
                src_name,
                dst_dir,
                dst_name,
                rustix::fs::RenameFlags::NOREPLACE,
            ) {
                Ok(()) => return Ok(()),
                // Not offered by this kernel or file system: fall through to
                // checking first.
                Err(e) if e == Errno::NOSYS || e == Errno::INVAL || e == Errno::NOTSUP => {}
                Err(e) => return Err(fs_err(e)),
            }
            match statat(dst_dir, dst_name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(_) => return Err(FsError::failure("file exists")),
                Err(e) if e == Errno::NOENT => {}
                Err(e) => return Err(fs_err(e)),
            }
        }
        renameat(src_dir, src_name, dst_dir, dst_name).map_err(fs_err)
    }

    pub(super) fn read_link(&self, path: &[u8]) -> Result<Vec<u8>, FsError> {
        let loc = self.walk(path, false)?;
        let (dir, name) = loc.leaf()?;
        let target = readlinkat(dir, name, Vec::new()).map_err(fs_err)?;
        Ok(target.into_bytes())
    }

    pub(super) fn symlink(&self, target: &[u8], link: &[u8]) -> Result<(), FsError> {
        if target.is_empty() || target.len() > MAX_PATH_LEN || target.contains(&0) {
            return Err(FsError::failure("invalid symbolic link target"));
        }
        let loc = self.walk(link, false)?;
        let (dir, name) = loc.leaf()?;
        symlinkat(target, dir, name).map_err(fs_err)
    }

    pub(super) fn hardlink(&self, existing: &[u8], new: &[u8]) -> Result<(), FsError> {
        let (src, dst) = (self.walk(existing, false)?, self.walk(new, false)?);
        let ((src_dir, src_name), (dst_dir, dst_name)) = (src.leaf()?, dst.leaf()?);
        let st = statat(src_dir, src_name, AtFlags::SYMLINK_NOFOLLOW).map_err(fs_err)?;
        if kind(&st) != FileType::RegularFile {
            return Err(FsError::failure("not a regular file"));
        }
        linkat(src_dir, src_name, dst_dir, dst_name, AtFlags::empty()).map_err(fs_err)
    }

    pub(super) fn statvfs(&self, path: &[u8]) -> Result<StatVfs, FsError> {
        let loc = self.walk(path, true)?;
        let (dir, name) = loc.target();
        let fd = openat(
            dir,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(fs_err)?;
        vfs_of(&fd)
    }
}

fn vfs_of(fd: &OwnedFd) -> Result<StatVfs, FsError> {
    let v = fstatvfs(fd).map_err(fs_err)?;
    let mut flags = 0;
    if v.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY) {
        flags |= StatVfs::READ_ONLY;
    }
    if v.f_flag.contains(rustix::fs::StatVfsMountFlags::NOSUID) {
        flags |= StatVfs::NO_SUID;
    }
    Ok(StatVfs {
        block_size: v.f_bsize as u64,
        fragment_size: v.f_frsize as u64,
        blocks: v.f_blocks as u64,
        blocks_free: v.f_bfree as u64,
        blocks_available: v.f_bavail as u64,
        files: v.f_files as u64,
        files_free: v.f_ffree as u64,
        files_available: v.f_favail as u64,
        id: v.f_fsid as u64,
        flags,
        name_max: v.f_namemax as u64,
    })
}

/// Reads a colon-separated account file such as `/etc/passwd` into a map
/// from numeric id (the third field) to name (the first). The first entry
/// for an id wins, as it does for the system's own lookups. An unreadable
/// file gives an empty map.
fn names_by_id(path: &str) -> HashMap<u32, Bytes> {
    let mut names = HashMap::new();
    for line in std::fs::read_to_string(path).unwrap_or_default().lines() {
        let mut fields = line.split(':');
        let name = fields.next();
        let id = fields.nth(1).and_then(|f| f.parse::<u32>().ok());
        if let (Some(name), Some(id)) = (name, id) {
            names
                .entry(id)
                .or_insert_with(|| Bytes::copy_from_slice(name.as_bytes()));
        }
    }
    names
}

/// User and group names for numeric ids, from the host's account files.
/// Unknown ids give empty names. Each file is read once per call, whatever
/// the number of ids asked for.
pub(super) fn users_groups(uids: &[u32], gids: &[u32]) -> (Vec<Bytes>, Vec<Bytes>) {
    let lookup = |path: &str, ids: &[u32]| {
        let names = names_by_id(path);
        ids.iter()
            .map(|id| names.get(id).cloned().unwrap_or_default())
            .collect()
    };
    (lookup("/etc/passwd", uids), lookup("/etc/group", gids))
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
        let n = self.file.read_at(&mut buf, offset).map_err(FsError::from)?;
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
            // The descriptor is in append mode: every write goes to the end.
            (&*self.file).write_all(data)
        } else {
            self.file.write_all_at(data, offset)
        }
        .map_err(FsError::from)
    }

    pub(super) fn fstat(&self) -> Result<Attributes, FsError> {
        Ok(attrs_of(&fstat(&*self.file).map_err(fs_err)?))
    }

    pub(super) fn fset(&self, attrs: &Attributes) -> Result<(), FsError> {
        if let Some(size) = attrs.size {
            if !self.write {
                return Err(FsError::permission_denied());
            }
            ftruncate(&*self.file, size).map_err(fs_err)?;
        }
        apply_to_fd(self.file.as_fd(), attrs)
    }

    pub(super) fn sync(&self) -> Result<(), FsError> {
        self.file.sync_all().map_err(FsError::from)
    }

    pub(super) fn statvfs(&self) -> Result<StatVfs, FsError> {
        let fd = self
            .file
            .as_fd()
            .try_clone_to_owned()
            .map_err(FsError::from)?;
        vfs_of(&fd)
    }
}

/// An open directory.
#[derive(Clone)]
pub struct LocalDir {
    dir: Arc<Mutex<Dir>>,
}

impl LocalDir {
    pub(super) fn read_batch(&self) -> Result<Option<Vec<NameEntry>>, FsError> {
        let mut dir = self.dir.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = Vec::new();
        while out.len() < DIR_BATCH {
            let entry = match dir.read() {
                None => break,
                Some(Err(e)) => return Err(fs_err(e)),
                Some(Ok(entry)) => entry,
            };
            let st = dir
                .fd()
                .and_then(|fd| statat(fd, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW));
            // An entry that vanished since it was listed is skipped.
            let Ok(st) = st else { continue };
            out.push(NameEntry {
                name: Bytes::copy_from_slice(entry.file_name().to_bytes()),
                long_name: Bytes::new(),
                attrs: attrs_of(&st),
            });
        }
        Ok(if out.is_empty() { None } else { Some(out) })
    }
}
