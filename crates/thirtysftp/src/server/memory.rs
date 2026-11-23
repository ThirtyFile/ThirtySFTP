//! A file system that lives in memory.
//!
//! [`MemoryFs`] exists for tests, examples and as a worked implementation of
//! [`Filesystem`]. It supports every version 3 operation and all the
//! extensions the server can dispatch. File contents are sparse, so a write at
//! a huge offset costs only the blocks it touches.
//!
//! Limits of the model: symbolic links are followed at the last path
//! component only, never in the middle of a path, and there is no per-user
//! permission checking: every client is the owner of everything.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use super::filesystem::{Filesystem, FsError};
use crate::extensions::{self as ext, StatVfs};
use crate::wire::{Attributes, NameEntry, OpenFlags, Owner, Times};

const BLOCK: u64 = 4096;
const TYPE_FILE: u32 = 0o100_000;
const TYPE_DIR: u32 = 0o040_000;
const TYPE_LINK: u32 = 0o120_000;
const MAX_LINK_HOPS: usize = 16;

fn now() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u32::try_from(d.as_secs()).unwrap_or(u32::MAX))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Contents of a file: only blocks that were written exist.
#[derive(Default)]
struct Sparse {
    size: u64,
    blocks: HashMap<u64, Box<[u8]>>,
}

impl Sparse {
    fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), FsError> {
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| FsError::failure("offset too large"))?;
        let mut pos = offset;
        let mut rest = data;
        while !rest.is_empty() {
            let within = (pos % BLOCK) as usize;
            let n = rest.len().min(BLOCK as usize - within);
            let block = self
                .blocks
                .entry(pos / BLOCK)
                .or_insert_with(|| vec![0; BLOCK as usize].into_boxed_slice());
            block[within..within + n].copy_from_slice(&rest[..n]);
            rest = &rest[n..];
            pos += n as u64;
        }
        self.size = self.size.max(end);
        Ok(())
    }

    fn read(&self, offset: u64, len: usize) -> Vec<u8> {
        let end = self.size.min(offset.saturating_add(len as u64));
        let mut out = Vec::new();
        let mut pos = offset;
        while pos < end {
            let within = (pos % BLOCK) as usize;
            let n = ((end - pos) as usize).min(BLOCK as usize - within);
            match self.blocks.get(&(pos / BLOCK)) {
                Some(b) => out.extend_from_slice(&b[within..within + n]),
                None => out.resize(out.len() + n, 0),
            }
            pos += n as u64;
        }
        out
    }

    fn set_len(&mut self, size: u64) {
        if size < self.size {
            self.blocks.retain(|&b, _| b * BLOCK < size);
            if let Some(b) = self.blocks.get_mut(&(size / BLOCK)) {
                b[(size % BLOCK) as usize..].fill(0);
            }
        }
        self.size = size;
    }
}

#[derive(Clone, Copy)]
struct Meta {
    mode: u32,
    uid: u32,
    gid: u32,
    atime: u32,
    mtime: u32,
}

impl Meta {
    fn new(mode: u32) -> Self {
        let t = now();
        Self {
            mode,
            uid: 0,
            gid: 0,
            atime: t,
            mtime: t,
        }
    }

    fn apply(&mut self, attrs: &Attributes) {
        if let Some(mode) = attrs.permissions {
            self.mode = mode & 0o7777;
        }
        if let Some(o) = attrs.owner {
            self.uid = o.uid;
            self.gid = o.gid;
        }
        if let Some(t) = attrs.times {
            self.atime = t.accessed;
            self.mtime = t.modified;
        }
    }

    fn attrs(&self, kind: u32, size: Option<u64>) -> Attributes {
        Attributes {
            size,
            owner: Some(Owner {
                uid: self.uid,
                gid: self.gid,
            }),
            permissions: Some(kind | self.mode),
            times: Some(Times {
                accessed: self.atime,
                modified: self.mtime,
            }),
            extended: Vec::new(),
        }
    }
}

struct FileNode {
    data: Sparse,
    meta: Meta,
}

#[derive(Clone)]
enum Node {
    File(Arc<Mutex<FileNode>>),
    Dir(Meta),
    Symlink(Vec<u8>, Meta),
}

impl Node {
    fn attrs(&self) -> Attributes {
        match self {
            Self::File(f) => {
                let f = lock(f);
                f.meta.attrs(TYPE_FILE, Some(f.data.size))
            }
            Self::Dir(m) => m.attrs(TYPE_DIR, None),
            Self::Symlink(t, m) => m.attrs(TYPE_LINK, Some(t.len() as u64)),
        }
    }

    fn apply(&mut self, attrs: &Attributes) -> Result<(), FsError> {
        match self {
            Self::File(f) => {
                let mut f = lock(f);
                if let Some(size) = attrs.size {
                    f.data.set_len(size);
                    f.meta.mtime = now();
                }
                f.meta.apply(attrs);
            }
            Self::Dir(m) | Self::Symlink(_, m) => m.apply(attrs),
        }
        Ok(())
    }
}

/// An open file of a [`MemoryFs`].
pub struct MemoryFile {
    node: Arc<Mutex<FileNode>>,
    read: bool,
    write: bool,
    append: bool,
}

/// An open directory of a [`MemoryFs`]: a snapshot taken when it was opened.
pub struct MemoryDir {
    entries: Mutex<Option<Vec<NameEntry>>>,
}

/// A file system kept in memory. See the [module documentation](self).
pub struct MemoryFs {
    nodes: Mutex<BTreeMap<Vec<u8>, Node>>,
}

impl Default for MemoryFs {
    fn default() -> Self {
        Self::new()
    }
}

/// Makes a path absolute and removes `.`, `..` and repeated slashes. A `..`
/// at the root stays at the root. Relative paths are taken from the root.
fn normalize(path: &[u8]) -> Vec<u8> {
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in path.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    let mut out = Vec::new();
    for p in &parts {
        out.push(b'/');
        out.extend_from_slice(p);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

fn parent_of(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|&b| b == b'/') {
        None | Some(0) => b"/".to_vec(),
        Some(i) => path[..i].to_vec(),
    }
}

fn base_name(path: &[u8]) -> &[u8] {
    path.rsplit(|&b| b == b'/').next().unwrap_or(path)
}

impl MemoryFs {
    /// An empty file system holding only the root directory `/`.
    pub fn new() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(b"/".to_vec(), Node::Dir(Meta::new(0o755)));
        Self {
            nodes: Mutex::new(nodes),
        }
    }

    /// Creates or replaces the file at `path` with `data`. The parent
    /// directory must exist.
    pub fn insert_file(&self, path: impl AsRef<[u8]>, data: &[u8]) -> Result<(), FsError> {
        let path = normalize(path.as_ref());
        let mut nodes = lock(&self.nodes);
        Self::require_parent(&nodes, &path)?;
        let mut content = FileNode {
            data: Sparse::default(),
            meta: Meta::new(0o644),
        };
        content.data.write(0, data)?;
        nodes.insert(path, Node::File(Arc::new(Mutex::new(content))));
        Ok(())
    }

    /// The contents of the regular file at `path`, if there is one. Reads the
    /// whole file into memory.
    pub fn file_contents(&self, path: impl AsRef<[u8]>) -> Option<Vec<u8>> {
        let nodes = lock(&self.nodes);
        match nodes.get(&normalize(path.as_ref()))? {
            Node::File(f) => {
                let f = lock(f);
                Some(f.data.read(0, usize::try_from(f.data.size).ok()?))
            }
            _ => None,
        }
    }

    /// Whether anything exists at `path`, links not followed.
    pub fn exists(&self, path: impl AsRef<[u8]>) -> bool {
        lock(&self.nodes).contains_key(&normalize(path.as_ref()))
    }

    /// Everything inside the directory `dir`, at any depth. The map is ordered,
    /// so these are the keys that follow `dir/`.
    fn below<'a>(
        nodes: &'a BTreeMap<Vec<u8>, Node>,
        dir: &[u8],
    ) -> impl Iterator<Item = (&'a Vec<u8>, &'a Node)> {
        let mut prefix = dir.to_vec();
        if prefix != b"/" {
            prefix.push(b'/');
        }
        // For the root the prefix is `/`, which is also the root's own key.
        let len = prefix.len();
        nodes
            .range(prefix.clone()..)
            .take_while(move |(key, _)| key.starts_with(&prefix))
            .filter(move |(key, _)| key.len() > len)
    }

    fn require_parent(nodes: &BTreeMap<Vec<u8>, Node>, path: &[u8]) -> Result<(), FsError> {
        match nodes.get(&parent_of(path)) {
            Some(Node::Dir(_)) => Ok(()),
            Some(_) => Err(FsError::failure("parent is not a directory")),
            None => Err(FsError::no_such_file()),
        }
    }

    /// Follows a link at the last component, up to a fixed number of hops.
    fn resolve(nodes: &BTreeMap<Vec<u8>, Node>, path: &[u8]) -> Result<Vec<u8>, FsError> {
        let mut path = normalize(path);
        for _ in 0..MAX_LINK_HOPS {
            match nodes.get(&path) {
                Some(Node::Symlink(target, _)) => {
                    path = if target.starts_with(b"/") {
                        normalize(target)
                    } else {
                        let mut joined = parent_of(&path);
                        joined.push(b'/');
                        joined.extend_from_slice(target);
                        normalize(&joined)
                    };
                }
                _ => return Ok(path),
            }
        }
        Err(FsError::failure("too many levels of symbolic links"))
    }

    fn get(&self, path: &[u8], follow: bool) -> Result<Node, FsError> {
        let nodes = lock(&self.nodes);
        let path = if follow {
            Self::resolve(&nodes, path)?
        } else {
            normalize(path)
        };
        nodes.get(&path).cloned().ok_or_else(FsError::no_such_file)
    }

    fn set_attrs(&self, path: &[u8], follow: bool, attrs: &Attributes) -> Result<(), FsError> {
        let mut nodes = lock(&self.nodes);
        let path = if follow {
            Self::resolve(&nodes, path)?
        } else {
            normalize(path)
        };
        nodes
            .get_mut(&path)
            .ok_or_else(FsError::no_such_file)?
            .apply(attrs)
    }

    fn do_rename(&self, from: &[u8], to: &[u8], replace: bool) -> Result<(), FsError> {
        let (from, to) = (normalize(from), normalize(to));
        let mut nodes = lock(&self.nodes);
        if from == b"/" || !nodes.contains_key(&from) {
            return Err(FsError::no_such_file());
        }
        if from == to {
            return Ok(());
        }
        let mut inside = from.clone();
        inside.push(b'/');
        if to.starts_with(&inside) {
            return Err(FsError::failure("cannot move a directory into itself"));
        }
        Self::require_parent(&nodes, &to)?;
        match nodes.get(&to) {
            None => {}
            Some(Node::Dir(_)) => return Err(FsError::failure("target is a directory")),
            Some(_) if !replace => return Err(FsError::failure("target exists")),
            Some(_) => {}
        }
        if matches!(nodes.get(&from), Some(Node::Dir(_))) && nodes.contains_key(&to) {
            return Err(FsError::failure("target exists"));
        }
        let moved: Vec<Vec<u8>> = nodes
            .range(from.clone()..)
            .map(|(k, _)| k.clone())
            .take_while(|k| *k == from || k.starts_with(&inside))
            .collect();
        for key in moved {
            if let Some(node) = nodes.remove(&key) {
                let mut new_key = to.clone();
                new_key.extend_from_slice(&key[from.len()..]);
                nodes.insert(new_key, node);
            }
        }
        Ok(())
    }
}

impl Filesystem for MemoryFs {
    type File = MemoryFile;
    type Dir = MemoryDir;

    async fn open(
        &self,
        path: &[u8],
        flags: OpenFlags,
        attrs: &Attributes,
    ) -> Result<MemoryFile, FsError> {
        let write = flags.contains(OpenFlags::WRITE) || flags.contains(OpenFlags::APPEND);
        let read = flags.contains(OpenFlags::READ);
        if !read && !write {
            return Err(FsError::failure("open needs read or write"));
        }
        let mut nodes = lock(&self.nodes);
        let resolved = Self::resolve(&nodes, path)?;
        let node = match nodes.get(&resolved) {
            Some(Node::File(f)) => {
                if flags.contains(OpenFlags::CREATE) && flags.contains(OpenFlags::EXCLUSIVE) {
                    return Err(FsError::failure("file exists"));
                }
                if write && flags.contains(OpenFlags::TRUNCATE) {
                    let mut f = lock(f);
                    f.data.set_len(0);
                    f.meta.mtime = now();
                }
                f.clone()
            }
            Some(_) => return Err(FsError::failure("not a regular file")),
            None if flags.contains(OpenFlags::CREATE) => {
                Self::require_parent(&nodes, &resolved)?;
                let mut meta = Meta::new(0o644);
                meta.apply(&Attributes {
                    permissions: attrs.permissions,
                    ..Attributes::default()
                });
                let node = Arc::new(Mutex::new(FileNode {
                    data: Sparse::default(),
                    meta,
                }));
                nodes.insert(resolved, Node::File(node.clone()));
                node
            }
            None => return Err(FsError::no_such_file()),
        };
        Ok(MemoryFile {
            node,
            read,
            write,
            append: flags.contains(OpenFlags::APPEND),
        })
    }

    async fn read(
        &self,
        file: &MemoryFile,
        offset: u64,
        len: u32,
    ) -> Result<Option<Bytes>, FsError> {
        if !file.read {
            return Err(FsError::permission_denied());
        }
        let node = lock(&file.node);
        if offset >= node.data.size {
            return Ok(None);
        }
        Ok(Some(node.data.read(offset, len as usize).into()))
    }

    async fn write(&self, file: &MemoryFile, offset: u64, data: Bytes) -> Result<(), FsError> {
        if !file.write {
            return Err(FsError::permission_denied());
        }
        let mut node = lock(&file.node);
        let at = if file.append { node.data.size } else { offset };
        node.data.write(at, &data)?;
        node.meta.mtime = now();
        Ok(())
    }

    async fn fstat(&self, file: &MemoryFile) -> Result<Attributes, FsError> {
        let node = lock(&file.node);
        Ok(node.meta.attrs(TYPE_FILE, Some(node.data.size)))
    }

    async fn fsetstat(&self, file: &MemoryFile, attrs: &Attributes) -> Result<(), FsError> {
        let mut node = lock(&file.node);
        if let Some(size) = attrs.size {
            if !file.write {
                return Err(FsError::permission_denied());
            }
            node.data.set_len(size);
            node.meta.mtime = now();
        }
        node.meta.apply(attrs);
        Ok(())
    }

    async fn stat(&self, path: &[u8]) -> Result<Attributes, FsError> {
        Ok(self.get(path, true)?.attrs())
    }

    async fn lstat(&self, path: &[u8]) -> Result<Attributes, FsError> {
        Ok(self.get(path, false)?.attrs())
    }

    async fn setstat(&self, path: &[u8], attrs: &Attributes) -> Result<(), FsError> {
        self.set_attrs(path, true, attrs)
    }

    async fn open_dir(&self, path: &[u8]) -> Result<MemoryDir, FsError> {
        let nodes = lock(&self.nodes);
        let dir = Self::resolve(&nodes, path)?;
        match nodes.get(&dir) {
            Some(Node::Dir(_)) => {}
            Some(_) => return Err(FsError::failure("not a directory")),
            None => return Err(FsError::no_such_file()),
        }
        let entry = |name: &[u8], node: &Node| NameEntry {
            name: Bytes::copy_from_slice(name),
            long_name: Bytes::new(),
            attrs: node.attrs(),
        };
        let own = &nodes[&dir];
        let up = nodes.get(&parent_of(&dir)).unwrap_or(own);
        let mut entries = vec![entry(b".", own), entry(b"..", up)];
        for (key, node) in Self::below(&nodes, &dir) {
            if parent_of(key) == dir {
                entries.push(entry(base_name(key), node));
            }
        }
        Ok(MemoryDir {
            entries: Mutex::new(Some(entries)),
        })
    }

    async fn read_dir(&self, dir: &MemoryDir) -> Result<Option<Vec<NameEntry>>, FsError> {
        Ok(lock(&dir.entries).take())
    }

    async fn remove(&self, path: &[u8]) -> Result<(), FsError> {
        let path = normalize(path);
        let mut nodes = lock(&self.nodes);
        match nodes.get(&path) {
            Some(Node::File(_) | Node::Symlink(..)) => {
                nodes.remove(&path);
                Ok(())
            }
            Some(Node::Dir(_)) => Err(FsError::failure("is a directory")),
            None => Err(FsError::no_such_file()),
        }
    }

    async fn mkdir(&self, path: &[u8], attrs: &Attributes) -> Result<(), FsError> {
        let path = normalize(path);
        let mut nodes = lock(&self.nodes);
        if nodes.contains_key(&path) {
            return Err(FsError::failure("file exists"));
        }
        Self::require_parent(&nodes, &path)?;
        let mut meta = Meta::new(0o755);
        meta.apply(&Attributes {
            permissions: attrs.permissions,
            ..Attributes::default()
        });
        nodes.insert(path, Node::Dir(meta));
        Ok(())
    }

    async fn rmdir(&self, path: &[u8]) -> Result<(), FsError> {
        let path = normalize(path);
        let mut nodes = lock(&self.nodes);
        match nodes.get(&path) {
            Some(Node::Dir(_)) if path == b"/" => Err(FsError::failure("cannot remove the root")),
            Some(Node::Dir(_)) => {
                if Self::below(&nodes, &path).next().is_some() {
                    return Err(FsError::failure("directory not empty"));
                }
                nodes.remove(&path);
                Ok(())
            }
            Some(_) => Err(FsError::failure("not a directory")),
            None => Err(FsError::no_such_file()),
        }
    }

    async fn realpath(&self, path: &[u8]) -> Result<Bytes, FsError> {
        let nodes = lock(&self.nodes);
        Ok(Self::resolve(&nodes, path)?.into())
    }

    async fn rename(&self, from: &[u8], to: &[u8]) -> Result<(), FsError> {
        self.do_rename(from, to, false)
    }

    async fn read_link(&self, path: &[u8]) -> Result<Bytes, FsError> {
        match self.get(path, false)? {
            Node::Symlink(target, _) => Ok(target.into()),
            _ => Err(FsError::failure("not a symbolic link")),
        }
    }

    async fn symlink(&self, target: &[u8], link: &[u8]) -> Result<(), FsError> {
        let link = normalize(link);
        let mut nodes = lock(&self.nodes);
        if nodes.contains_key(&link) {
            return Err(FsError::failure("file exists"));
        }
        Self::require_parent(&nodes, &link)?;
        nodes.insert(link, Node::Symlink(target.to_vec(), Meta::new(0o777)));
        Ok(())
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[
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
        ]
    }

    async fn posix_rename(&self, from: &[u8], to: &[u8]) -> Result<(), FsError> {
        self.do_rename(from, to, true)
    }

    async fn hardlink(&self, existing: &[u8], new: &[u8]) -> Result<(), FsError> {
        let new = normalize(new);
        let mut nodes = lock(&self.nodes);
        let node = match nodes.get(&normalize(existing)) {
            Some(node @ Node::File(_)) => node.clone(),
            Some(_) => return Err(FsError::failure("not a regular file")),
            None => return Err(FsError::no_such_file()),
        };
        if nodes.contains_key(&new) {
            return Err(FsError::failure("file exists"));
        }
        Self::require_parent(&nodes, &new)?;
        nodes.insert(new, node);
        Ok(())
    }

    async fn fsync(&self, _file: &MemoryFile) -> Result<(), FsError> {
        Ok(())
    }

    async fn lsetstat(&self, path: &[u8], attrs: &Attributes) -> Result<(), FsError> {
        self.set_attrs(path, false, attrs)
    }

    async fn statvfs(&self, _path: &[u8]) -> Result<StatVfs, FsError> {
        Ok(StatVfs {
            block_size: BLOCK,
            fragment_size: BLOCK,
            blocks: 1 << 20,
            blocks_free: 1 << 19,
            blocks_available: 1 << 19,
            files: 1 << 20,
            files_free: 1 << 19,
            files_available: 1 << 19,
            id: 1,
            flags: 0,
            name_max: 255,
        })
    }

    async fn fstatvfs(&self, _file: &MemoryFile) -> Result<StatVfs, FsError> {
        self.statvfs(b"/").await
    }

    async fn expand_path(&self, path: &[u8]) -> Result<Bytes, FsError> {
        let rest = match path {
            b"~" => &b""[..],
            p if p.starts_with(b"~/") => &p[2..],
            p => p,
        };
        Ok(normalize(rest).into())
    }

    async fn home_directory(&self, _user: &[u8]) -> Result<Bytes, FsError> {
        Ok(Bytes::from_static(b"/"))
    }

    async fn users_groups_by_id(
        &self,
        uids: &[u32],
        gids: &[u32],
    ) -> Result<(Vec<Bytes>, Vec<Bytes>), FsError> {
        let name = |prefix: &str, id: u32| {
            if id == 0 {
                Bytes::from_static(b"root")
            } else {
                Bytes::from(format!("{prefix}{id}"))
            }
        };
        Ok((
            uids.iter().map(|&u| name("user", u)).collect(),
            gids.iter().map(|&g| name("group", g)).collect(),
        ))
    }
}
