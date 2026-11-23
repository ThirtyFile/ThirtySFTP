//! Helpers shared by the test files: an in-memory SFTP server that stores
//! files (and can answer reads short, delay and reorder replies, fail a chosen
//! write and state limits), a scripted server driven packet by packet, a
//! scratch directory, and a fixed-seed byte generator.
#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use thirtysftp::client::{Config, Session};
use thirtysftp::codec::FrameReader;
use thirtysftp::wire::{
    Attributes, ClientPacket, Encode, Extension, Hello, NameEntry, Op, OpenFlags, Reply, Request,
    Response, ServerPacket, StatusCode,
};
use tokio::io::{AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf, duplex, split};
use tokio::sync::mpsc;

const BLOCK: u64 = 4096;

/// Behaviour switches.
#[derive(Clone)]
pub struct Opts {
    /// Most bytes returned for one read.
    pub max_read: usize,
    /// Reject writes longer than this.
    pub max_write: usize,
    /// Answer every read with empty data.
    pub empty_reads: bool,
    /// Delay replies by a random few milliseconds, which reorders them.
    pub delay: bool,
    /// Fail the n-th write request (counting from 1) with `Failure`.
    pub fail_write: Option<usize>,
    /// Announce `limits@openssh.com` with these values: packet, read, write,
    /// handles.
    pub limits: Option<[u64; 4]>,
    /// Announce `posix-rename@openssh.com`.
    pub posix_rename: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            max_read: usize::MAX,
            max_write: usize::MAX,
            empty_reads: false,
            delay: false,
            fail_write: None,
            limits: None,
            posix_rename: false,
        }
    }
}

/// A sparse file: only blocks that were written exist.
#[derive(Default)]
pub struct FileData {
    size: u64,
    blocks: HashMap<u64, Vec<u8>>,
}

impl FileData {
    pub fn from_bytes(data: &[u8]) -> Self {
        let mut f = Self::default();
        f.write(0, data);
        f
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn write(&mut self, offset: u64, data: &[u8]) {
        let mut pos = offset;
        let mut rest = data;
        while !rest.is_empty() {
            let block = pos / BLOCK;
            let within = (pos % BLOCK) as usize;
            let n = rest.len().min(BLOCK as usize - within);
            let b = self
                .blocks
                .entry(block)
                .or_insert_with(|| vec![0; BLOCK as usize]);
            b[within..within + n].copy_from_slice(&rest[..n]);
            rest = &rest[n..];
            pos += n as u64;
        }
        self.size = self.size.max(offset + data.len() as u64);
    }

    pub fn read(&self, offset: u64, len: usize) -> Vec<u8> {
        let end = self.size.min(offset + len as u64);
        let mut out = Vec::new();
        let mut pos = offset;
        while pos < end {
            let block = pos / BLOCK;
            let within = (pos % BLOCK) as usize;
            let n = ((end - pos) as usize).min(BLOCK as usize - within);
            match self.blocks.get(&block) {
                Some(b) => out.extend_from_slice(&b[within..within + n]),
                None => out.resize(out.len() + n, 0),
            }
            pos += n as u64;
        }
        out
    }

    pub fn contents(&self) -> Vec<u8> {
        self.read(0, self.size as usize)
    }

    fn truncate(&mut self, size: u64) {
        if size < self.size {
            self.blocks.retain(|&b, _| b * BLOCK < size);
            if let Some(b) = self.blocks.get_mut(&(size / BLOCK)) {
                for x in &mut b[(size % BLOCK) as usize..] {
                    *x = 0;
                }
            }
        }
        self.size = size;
    }
}

enum Open {
    File {
        path: Vec<u8>,
        append: bool,
    },
    Dir {
        entries: Vec<NameEntry>,
        next: usize,
    },
}

#[derive(Default)]
pub struct Fs {
    pub files: HashMap<Vec<u8>, FileData>,
    pub dirs: BTreeSet<Vec<u8>>,
    handles: HashMap<Vec<u8>, Open>,
    next_handle: u64,
    writes: usize,
    pub max_read_seen: usize,
    pub max_write_seen: usize,
    pub requests: usize,
}

impl Fs {
    pub fn open_handles(&self) -> usize {
        self.handles.len()
    }

    pub fn put(&mut self, path: &str, data: &[u8]) {
        self.files
            .insert(path.as_bytes().to_vec(), FileData::from_bytes(data));
    }

    pub fn contents(&self, path: &str) -> Vec<u8> {
        self.files[path.as_bytes()].contents()
    }
}

pub type SharedFs = Arc<Mutex<Fs>>;

fn status(code: StatusCode) -> Reply {
    Reply::Status {
        code,
        message: Bytes::from_static(b"test"),
        language: Bytes::new(),
    }
}

fn parent(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|&b| b == b'/') {
        Some(0) | None => b"/".to_vec(),
        Some(i) => path[..i].to_vec(),
    }
}

fn attrs_of(fs: &Fs, path: &[u8]) -> Option<Attributes> {
    if let Some(f) = fs.files.get(path) {
        Some(Attributes {
            size: Some(f.size),
            permissions: Some(0o100_644),
            ..Attributes::default()
        })
    } else if path == b"/" || fs.dirs.contains(path) {
        Some(Attributes {
            permissions: Some(0o040_755),
            ..Attributes::default()
        })
    } else {
        None
    }
}

fn handle_request(fs: &mut Fs, opts: &Opts, op: Op) -> Reply {
    fs.requests += 1;
    match op {
        Op::Open { path, flags, .. } => {
            let path = path.to_vec();
            let exists = fs.files.contains_key(&path);
            if flags.contains(OpenFlags::CREATE) {
                if exists && flags.contains(OpenFlags::EXCLUSIVE) {
                    return status(StatusCode::Failure);
                }
                fs.files.entry(path.clone()).or_default();
            } else if !exists {
                return status(StatusCode::NoSuchFile);
            }
            if flags.contains(OpenFlags::TRUNCATE) {
                fs.files.get_mut(&path).unwrap().truncate(0);
            }
            fs.next_handle += 1;
            let h = format!("h{}", fs.next_handle).into_bytes();
            fs.handles.insert(
                h.clone(),
                Open::File {
                    path,
                    append: flags.contains(OpenFlags::APPEND),
                },
            );
            Reply::Handle(h.into())
        }
        Op::Close { handle } => match fs.handles.remove(handle.as_ref()) {
            Some(_) => status(StatusCode::Ok),
            None => status(StatusCode::Failure),
        },
        Op::Read {
            handle,
            offset,
            len,
        } => {
            fs.max_read_seen = fs.max_read_seen.max(len as usize);
            let Some(Open::File { path, .. }) = fs.handles.get(handle.as_ref()) else {
                return status(StatusCode::Failure);
            };
            if opts.empty_reads {
                return Reply::Data(Bytes::new());
            }
            let file = &fs.files[path];
            if offset >= file.size {
                return status(StatusCode::Eof);
            }
            let n = (len as usize).min(opts.max_read);
            Reply::Data(file.read(offset, n).into())
        }
        Op::Write {
            handle,
            offset,
            data,
        } => {
            fs.writes += 1;
            fs.max_write_seen = fs.max_write_seen.max(data.len());
            if opts.fail_write == Some(fs.writes) || data.len() > opts.max_write {
                return status(StatusCode::Failure);
            }
            let Some(Open::File { path, append }) = fs.handles.get(handle.as_ref()) else {
                return status(StatusCode::Failure);
            };
            let (path, append) = (path.clone(), *append);
            let file = fs.files.get_mut(&path).unwrap();
            let at = if append { file.size } else { offset };
            file.write(at, &data);
            status(StatusCode::Ok)
        }
        Op::Stat { path } | Op::Lstat { path } => match attrs_of(fs, &path) {
            Some(a) => Reply::Attrs(a),
            None => status(StatusCode::NoSuchFile),
        },
        Op::Fstat { handle } => match fs.handles.get(handle.as_ref()) {
            Some(Open::File { path, .. }) => Reply::Attrs(attrs_of(fs, path).unwrap()),
            _ => status(StatusCode::Failure),
        },
        Op::SetStat { path, attrs } => match fs.files.get_mut(path.as_ref()) {
            Some(f) => {
                if let Some(size) = attrs.size {
                    f.truncate(size);
                }
                status(StatusCode::Ok)
            }
            None => status(StatusCode::NoSuchFile),
        },
        Op::FsetStat { handle, attrs } => match fs.handles.get(handle.as_ref()) {
            Some(Open::File { path, .. }) => {
                let path = path.clone();
                if let Some(size) = attrs.size {
                    fs.files.get_mut(&path).unwrap().truncate(size);
                }
                status(StatusCode::Ok)
            }
            _ => status(StatusCode::Failure),
        },
        Op::OpenDir { path } => {
            let path = path.to_vec();
            if path != b"/" && !fs.dirs.contains(&path) {
                return status(StatusCode::NoSuchFile);
            }
            let entry = |name: &[u8]| NameEntry {
                name: Bytes::copy_from_slice(name),
                long_name: Bytes::new(),
                attrs: Attributes::default(),
            };
            let mut entries = vec![entry(b"."), entry(b"..")];
            let base = |p: &Vec<u8>| p.rsplit(|&b| b == b'/').next().unwrap().to_vec();
            for p in fs.dirs.iter().chain(fs.files.keys()) {
                if *p != path && parent(p) == path {
                    entries.push(entry(&base(p)));
                }
            }
            fs.next_handle += 1;
            let h = format!("d{}", fs.next_handle).into_bytes();
            fs.handles.insert(h.clone(), Open::Dir { entries, next: 0 });
            Reply::Handle(h.into())
        }
        Op::ReadDir { handle } => match fs.handles.get_mut(handle.as_ref()) {
            Some(Open::Dir { entries, next }) => {
                if *next >= entries.len() {
                    return status(StatusCode::Eof);
                }
                let end = (*next + 3).min(entries.len());
                let batch = entries[*next..end].to_vec();
                *next = end;
                Reply::Name(batch)
            }
            _ => status(StatusCode::Failure),
        },
        Op::Remove { path } => match fs.files.remove(path.as_ref()) {
            Some(_) => status(StatusCode::Ok),
            None => status(StatusCode::NoSuchFile),
        },
        Op::MkDir { path, .. } => {
            let path = path.to_vec();
            if fs.dirs.contains(&path) || fs.files.contains_key(&path) {
                return status(StatusCode::Failure);
            }
            fs.dirs.insert(path);
            status(StatusCode::Ok)
        }
        Op::RmDir { path } => {
            if fs.dirs.remove(path.as_ref()) {
                status(StatusCode::Ok)
            } else {
                status(StatusCode::NoSuchFile)
            }
        }
        Op::RealPath { path } => Reply::Name(vec![NameEntry {
            name: if path == "." {
                Bytes::from_static(b"/")
            } else {
                path
            },
            long_name: Bytes::new(),
            attrs: Attributes::default(),
        }]),
        Op::Rename { from, to } => {
            if fs.files.contains_key(to.as_ref()) {
                return status(StatusCode::Failure);
            }
            match fs.files.remove(from.as_ref()) {
                Some(f) => {
                    fs.files.insert(to.to_vec(), f);
                    status(StatusCode::Ok)
                }
                None => status(StatusCode::NoSuchFile),
            }
        }
        Op::Extended { name, data } => match &name[..] {
            b"limits@openssh.com" => {
                let l = opts.limits.unwrap_or([0; 4]);
                let mut out = BytesMut::new();
                for v in l {
                    out.put_u64(v);
                }
                Reply::Extended(out.freeze())
            }
            b"posix-rename@openssh.com" => {
                let mut d = &data[..];
                let mut take = || {
                    let n = u32::from_be_bytes(d[..4].try_into().unwrap()) as usize;
                    let s = d[4..4 + n].to_vec();
                    d = &d[4 + n..];
                    s
                };
                let (from, to) = (take(), take());
                match fs.files.remove(&from) {
                    Some(f) => {
                        fs.files.insert(to, f);
                        status(StatusCode::Ok)
                    }
                    None => status(StatusCode::NoSuchFile),
                }
            }
            _ => status(StatusCode::OpUnsupported),
        },
        Op::Symlink { .. } | Op::ReadLink { .. } | Op::Unknown { .. } => {
            status(StatusCode::OpUnsupported)
        }
    }
}

async fn serve(stream: tokio::io::DuplexStream, opts: Opts, fs: SharedFs) {
    let (r, mut w) = split(stream);
    let mut reader = FrameReader::new(r, 1 << 20);
    let Ok(Some(init)) = reader.read_frame().await else {
        return;
    };
    assert!(matches!(
        ClientPacket::decode(init).unwrap(),
        ClientPacket::Init(_)
    ));
    let mut extensions = Vec::new();
    if opts.limits.is_some() {
        extensions.push(Extension {
            name: Bytes::from_static(b"limits@openssh.com"),
            data: Bytes::from_static(b"1"),
        });
    }
    if opts.posix_rename {
        extensions.push(Extension {
            name: Bytes::from_static(b"posix-rename@openssh.com"),
            data: Bytes::from_static(b"1"),
        });
    }
    let mut out = BytesMut::new();
    ServerPacket::Version(Hello {
        version: 3,
        extensions,
    })
    .encode_frame(&mut out)
    .unwrap();
    w.write_all(&out).await.unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if w.write_all(&frame).await.is_err() {
                return;
            }
        }
    });

    let mut rng = 0x2545_f491_4f6c_dd1du64;
    while let Ok(Some(payload)) = reader.read_frame().await {
        let ClientPacket::Request(request) = ClientPacket::decode(payload).unwrap() else {
            panic!("second handshake");
        };
        let reply = handle_request(&mut fs.lock().unwrap(), &opts, request.op);
        let mut frame = BytesMut::new();
        ServerPacket::Response(Response {
            id: request.id,
            reply,
        })
        .encode_frame(&mut frame)
        .unwrap();
        let frame = frame.freeze();
        if opts.delay {
            rng ^= rng >> 12;
            rng ^= rng << 25;
            rng ^= rng >> 27;
            let ms = rng.wrapping_mul(0x2545_f491_4f6c_dd1d) % 6;
            let tx = tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                let _ = tx.send(frame);
            });
        } else {
            let _ = tx.send(frame);
        }
    }
}

/// Starts a server and connects a session to it.
pub async fn connect(opts: Opts, config: Config) -> (Session, SharedFs) {
    let fs: SharedFs = Arc::default();
    let (client, server) = duplex(1 << 20);
    tokio::spawn(serve(server, opts, fs.clone()));
    let (r, w) = split(client);
    (Session::connect(r, w, config).await.unwrap(), fs)
}

/// `len` bytes from a fixed-seed generator.
pub fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            (x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 56) as u8
        })
        .collect()
}

/// A server driven by the test, one packet at a time.
pub struct Scripted {
    pub reader: FrameReader<ReadHalf<DuplexStream>>,
    pub writer: WriteHalf<DuplexStream>,
}

impl Scripted {
    /// The next request the client sent.
    pub async fn request(&mut self) -> Request {
        let payload = self.reader.read_frame().await.unwrap().unwrap();
        match ClientPacket::decode(payload).unwrap() {
            ClientPacket::Request(r) => r,
            other => panic!("expected a request, got {other:?}"),
        }
    }

    pub async fn send(&mut self, packet: ServerPacket) {
        let mut out = BytesMut::new();
        packet.encode_frame(&mut out).unwrap();
        self.writer.write_all(&out).await.unwrap();
    }

    pub async fn reply(&mut self, id: u32, reply: Reply) {
        self.send(ServerPacket::Response(Response { id, reply }))
            .await;
    }

    pub async fn status(&mut self, id: u32, code: StatusCode) {
        self.reply(id, status_reply(code)).await;
    }
}

/// A status reply with a fixed message.
pub fn status_reply(code: StatusCode) -> Reply {
    Reply::Status {
        code,
        message: Bytes::from_static(b"msg"),
        language: Bytes::new(),
    }
}

/// Connects a session to a scripted server that announces `extensions`.
pub async fn scripted(config: Config, extensions: Vec<Extension>) -> (Session, Scripted) {
    let (client, server) = duplex(256 * 1024);
    let (cr, cw) = split(client);
    let (sr, sw) = split(server);
    let connect = tokio::spawn(Session::connect(cr, cw, config));
    let mut server = Scripted {
        reader: FrameReader::new(sr, 1 << 20),
        writer: sw,
    };
    let init = server.reader.read_frame().await.unwrap().unwrap();
    assert_eq!(
        ClientPacket::decode(init).unwrap(),
        ClientPacket::Init(Hello {
            version: 3,
            extensions: vec![]
        })
    );
    server
        .send(ServerPacket::Version(Hello {
            version: 3,
            extensions,
        }))
        .await;
    (connect.await.unwrap().unwrap(), server)
}

static SCRATCH: AtomicUsize = AtomicUsize::new(0);

/// A scratch directory, removed on drop.
pub struct Tmp(pub PathBuf);

impl Tmp {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "thirtysftp-test-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// A path inside the directory, as text.
    pub fn remote(&self, name: &str) -> String {
        self.path(name).to_str().unwrap().to_owned()
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
