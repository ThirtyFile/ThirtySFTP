//! The server, driven by the client in memory and by hand-written packets.
#![cfg(all(feature = "client", feature = "server"))]

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use common::random_bytes;
use thirtysftp::client::{Config, File, Session, fs};
use thirtysftp::codec::FrameReader;
use thirtysftp::server::{Filesystem, FsError, MemoryFs, ServerConfig, serve};
use thirtysftp::wire::{
    Attributes, ClientPacket, Encode, Hello, NameEntry, Op, OpenFlags, Reply, Request,
    ServerPacket, StatusCode,
};
use thirtysftp::{Error, ProtocolError};
use tokio::io::{AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf, duplex, split};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::timeout;

type ServeResult = Result<(), Error>;

fn spawn_server<F: Filesystem>(
    stream: DuplexStream,
    fs: Arc<F>,
    config: ServerConfig,
) -> JoinHandle<ServeResult> {
    let (r, w) = split(stream);
    tokio::spawn(serve(r, w, fs, config))
}

/// A client session on a server over `fs`.
async fn session_with<F: Filesystem>(
    fs: Arc<F>,
    config: ServerConfig,
) -> (Session, JoinHandle<ServeResult>) {
    let (client, server) = duplex(1 << 20);
    let handle = spawn_server(server, fs, config);
    let (r, w) = split(client);
    (
        Session::connect(r, w, Config::default()).await.unwrap(),
        handle,
    )
}

async fn session() -> (Session, Arc<MemoryFs>) {
    let fs = Arc::new(MemoryFs::new());
    let (s, _) = session_with(fs.clone(), ServerConfig::default()).await;
    (s, fs)
}

/// A client that speaks raw packets.
struct Raw {
    reader: FrameReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    server: JoinHandle<ServeResult>,
}

impl Raw {
    async fn start<F: Filesystem>(fs: Arc<F>, config: ServerConfig) -> Self {
        let (client, server) = duplex(1 << 20);
        let (r, w) = split(client);
        Self {
            reader: FrameReader::new(r, 1 << 22),
            writer: w,
            server: spawn_server(server, fs, config),
        }
    }

    async fn handshake(&mut self) -> Hello {
        self.send(&ClientPacket::Init(Hello {
            version: 3,
            extensions: vec![],
        }))
        .await;
        match self.recv().await {
            ServerPacket::Version(h) => h,
            other => panic!("{other:?}"),
        }
    }

    async fn send(&mut self, packet: &ClientPacket) {
        let mut out = BytesMut::new();
        packet.encode_frame(&mut out).unwrap();
        self.writer.write_all(&out).await.unwrap();
    }

    async fn request(&mut self, id: u32, op: Op) {
        self.send(&ClientPacket::Request(Request { id, op })).await;
    }

    async fn recv(&mut self) -> ServerPacket {
        let payload = timeout(Duration::from_secs(5), self.reader.read_frame())
            .await
            .expect("no reply")
            .unwrap()
            .expect("connection closed");
        ServerPacket::decode(payload).unwrap()
    }

    /// The reply to the request just sent.
    async fn reply(&mut self) -> (u32, Reply) {
        match self.recv().await {
            ServerPacket::Response(r) => (r.id, r.reply),
            other => panic!("{other:?}"),
        }
    }

    async fn status(&mut self) -> (u32, StatusCode) {
        match self.reply().await {
            (id, Reply::Status { code, .. }) => (id, code),
            other => panic!("{other:?}"),
        }
    }

    /// The server's result once the connection is over.
    async fn finish(self) -> ServeResult {
        drop(self.writer);
        drop(self.reader);
        timeout(Duration::from_secs(5), self.server)
            .await
            .expect("server did not stop")
            .unwrap()
    }
}

fn b(s: &str) -> Bytes {
    Bytes::copy_from_slice(s.as_bytes())
}

#[tokio::test]
async fn every_version_3_operation_works_end_to_end() {
    let (s, _fs) = session().await;
    let rw = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE;

    // Files: create, write, read, stat.
    let h = s.open("/f", rw, Attributes::default()).await.unwrap();
    s.write(&h, 0, b("hello world")).await.unwrap();
    assert_eq!(s.read(&h, 6, 100).await.unwrap(), Some(b("world")));
    assert_eq!(s.read(&h, 11, 10).await.unwrap(), None);
    assert_eq!(s.fstat(&h).await.unwrap().size, Some(11));
    s.fsetstat(
        &h,
        Attributes {
            size: Some(5),
            ..Attributes::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(s.read(&h, 0, 100).await.unwrap(), Some(b("hello")));
    s.close_handle(&h).await.unwrap();
    let attrs = s.stat("/f").await.unwrap();
    assert!(attrs.is_file());
    assert_eq!(attrs.size, Some(5));
    s.setstat(
        "/f",
        Attributes {
            permissions: Some(0o600),
            ..Attributes::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(s.stat("/f").await.unwrap().permissions, Some(0o100_600));

    // Directories.
    s.mkdir("/d", Attributes::default()).await.unwrap();
    assert!(s.stat("/d").await.unwrap().is_dir());
    assert!(s.mkdir("/d", Attributes::default()).await.is_err());
    s.rename("/f", "/d/g").await.unwrap();
    assert!(s.stat("/f").await.is_err());
    let dir = s.opendir("/d").await.unwrap();
    let mut names = Vec::new();
    while let Some(batch) = s.readdir(&dir).await.unwrap() {
        names.extend(batch.into_iter().map(|e| e.name));
    }
    s.close_handle(&dir).await.unwrap();
    names.sort();
    assert_eq!(names, [b("."), b(".."), b("g")]);
    assert!(s.rmdir("/d").await.is_err(), "directory is not empty");

    // Links and paths.
    s.symlink("g", "/d/link").await.unwrap();
    assert_eq!(s.readlink("/d/link").await.unwrap(), "g");
    assert!(s.lstat("/d/link").await.unwrap().is_symlink());
    assert!(s.stat("/d/link").await.unwrap().is_file());
    assert_eq!(s.realpath("/d/../d/./g").await.unwrap(), "/d/g");
    assert_eq!(s.realpath("/d/link").await.unwrap(), "/d/g");

    // Removal.
    s.remove("/d/link").await.unwrap();
    s.remove("/d/g").await.unwrap();
    s.rmdir("/d").await.unwrap();
    assert_eq!(
        s.stat("/d").await.unwrap_err().status_code(),
        Some(StatusCode::NoSuchFile)
    );
}

#[tokio::test]
async fn every_extension_works_end_to_end() {
    let (s, fs_) = session().await;
    fs_.insert_file("/a", b"alpha").unwrap();
    for name in MemoryFs::new().extensions() {
        assert!(s.supports(name), "{name} not announced");
        // The version OpenSSH's client expects for each.
        let version = if name.starts_with("statvfs") || name.starts_with("fstatvfs") {
            "2"
        } else {
            "1"
        };
        assert_eq!(s.extension(name).unwrap(), version, "{name}");
    }
    assert!(s.supports(thirtysftp::extensions::LIMITS));

    s.posix_rename("/a", "/b").await.unwrap();
    assert!(!fs_.exists("/a"));
    fs_.insert_file("/c", b"gamma").unwrap();
    s.posix_rename("/b", "/c").await.unwrap(); // replaces
    assert_eq!(fs_.file_contents("/c").unwrap(), b"alpha");

    s.hardlink("/c", "/linked").await.unwrap();
    let h = File::open(&s, "/linked").await.unwrap();
    assert_eq!(h.read_range(0, 100).await.unwrap(), b"alpha");
    h.sync_all().await.unwrap();
    let writer = thirtysftp::client::OpenOptions::new()
        .write(true)
        .open(&s, "/c")
        .await
        .unwrap();
    writer.write_all_at(0, b("ALPHA")).await.unwrap();
    // Both names show the write: they are one file.
    assert_eq!(h.read_range(0, 100).await.unwrap(), b"ALPHA");
    writer.close().await.unwrap();

    let vfs = s.statvfs("/").await.unwrap();
    assert_eq!(vfs.name_max, 255);
    assert_eq!(s.fstatvfs(h.handle()).await.unwrap(), vfs);

    s.lsetstat(
        "/c",
        &Attributes {
            permissions: Some(0o640),
            ..Attributes::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(s.stat("/c").await.unwrap().permissions, Some(0o100_640));

    assert_eq!(s.expand_path("~/x/../y").await.unwrap(), "/y");
    assert_eq!(s.home_directory("").await.unwrap(), "/");
    let (users, groups) = s.users_groups_by_id(&[0, 7], &[0, 9, 9]).await.unwrap();
    assert_eq!(users, [b("root"), b("user7")]);
    assert_eq!(groups, [b("root"), b("group9"), b("group9")]);

    // copy-data between two files on the server.
    fs_.insert_file("/src", &random_bytes(200_000, 1)).unwrap();
    let src = File::open(&s, "/src").await.unwrap();
    let dst = File::create(&s, "/dst").await.unwrap();
    s.copy_data(src.handle(), 1000, 150_000, dst.handle(), 0)
        .await
        .unwrap();
    assert_eq!(
        fs_.file_contents("/dst").unwrap(),
        random_bytes(200_000, 1)[1000..151_000]
    );
    // Length 0 copies to the end of the source.
    s.copy_data(src.handle(), 199_000, 0, dst.handle(), 150_000)
        .await
        .unwrap();
    let copied = fs_.file_contents("/dst").unwrap();
    assert_eq!(copied.len(), 151_000);
    assert_eq!(copied[150_000..], random_bytes(200_000, 1)[199_000..]);
    src.close().await.unwrap();
    dst.close().await.unwrap();
    // Copying a file onto itself must not deadlock the handle's queue.
    let both = File::open(&s, "/c").await.unwrap();
    let r = timeout(
        Duration::from_secs(5),
        s.copy_data(both.handle(), 0, 5, both.handle(), 0),
    )
    .await
    .expect("copy onto itself hung");
    assert!(r.is_ok() || r.is_err());
}

#[tokio::test]
async fn large_transfers_with_small_server_limits_are_byte_identical() {
    let fs_ = Arc::new(MemoryFs::new());
    let config = ServerConfig::default()
        .with_max_read_length(3000)
        .with_max_write_length(2000)
        .with_max_packet(16 * 1024);
    let (s, _server) = session_with(fs_.clone(), config).await;
    let limits = s.known_limits().copied().unwrap();
    assert_eq!(limits.max_read_length, 3000);
    assert_eq!(limits.max_write_length, 2000);
    assert_eq!(limits.max_packet_length, 16 * 1024);
    assert_eq!(limits.max_open_handles, 256);

    let data = random_bytes(1_000_000, 2);
    fs::write(&s, "/blob", data.clone()).await.unwrap();
    assert_eq!(fs_.file_contents("/blob").unwrap(), data);
    assert_eq!(fs::read(&s, "/blob").await.unwrap(), data);
}

#[tokio::test]
async fn a_server_with_smaller_limits_than_the_client_assumes_still_serves_correctly() {
    // A client that ignores limits sends big writes and asks for big reads.
    let fs_ = Arc::new(MemoryFs::new());
    let config = ServerConfig::default()
        .with_max_read_length(100)
        .with_max_write_length(100);
    let (s, _server) = session_with(fs_.clone(), config).await;
    let h = s
        .open(
            "/f",
            OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE,
            Attributes::default(),
        )
        .await
        .unwrap();
    // Too long a write is refused, not truncated.
    let err = s
        .write(&h, 0, Bytes::from(vec![1u8; 101]))
        .await
        .unwrap_err();
    assert_eq!(err.status_code(), Some(StatusCode::Failure));
    s.write(&h, 0, Bytes::from(vec![1u8; 100])).await.unwrap();
    // A long read is answered short, which the protocol allows.
    s.write(&h, 100, Bytes::from(vec![2u8; 100])).await.unwrap();
    let data = s.read(&h, 0, 150).await.unwrap().unwrap();
    assert_eq!(data.len(), 100);
}

#[tokio::test]
async fn offsets_beyond_four_gibibytes_work_through_the_server() {
    let (s, fs_) = session().await;
    let at = (6u64 << 30) + 17;
    let file = thirtysftp::client::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&s, "/big")
        .await
        .unwrap();
    file.write_all_at(at, b("far")).await.unwrap();
    assert_eq!(file.read_range(at, 3).await.unwrap(), b"far");
    assert_eq!(file.read_range(at - 2, 5).await.unwrap(), b"\0\0far");
    assert_eq!(file.metadata().await.unwrap().size, Some(at + 3));
    file.set_len(10).await.unwrap();
    assert!(file.read_range(at, 3).await.unwrap().is_empty());
    assert_eq!(fs_.file_contents("/big").unwrap().len(), 10);
}

#[tokio::test]
async fn the_handle_limit_holds_and_closing_frees_a_slot() {
    let fs_ = Arc::new(MemoryFs::new());
    let (s, _server) = session_with(fs_.clone(), ServerConfig::default().with_max_handles(3)).await;
    let mut open = Vec::new();
    for i in 0..3 {
        open.push(
            s.open(
                format!("/f{i}"),
                OpenFlags::WRITE | OpenFlags::CREATE,
                Attributes::default(),
            )
            .await
            .unwrap(),
        );
    }
    let err = s
        .open(
            "/f3",
            OpenFlags::WRITE | OpenFlags::CREATE,
            Attributes::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.status_code(), Some(StatusCode::Failure));
    // Directories count too.
    assert!(s.opendir("/").await.is_err());
    s.close_handle(&open.pop().unwrap()).await.unwrap();
    s.opendir("/").await.unwrap();
}

#[tokio::test]
async fn a_packet_over_the_limit_ends_the_connection_unread() {
    let mut raw = Raw::start(
        Arc::new(MemoryFs::new()),
        ServerConfig::default().with_max_packet(4096),
    )
    .await;
    raw.handshake().await;
    // Only the length field is sent. The server must give up on it alone.
    raw.writer
        .write_all(&0x0100_0000u32.to_be_bytes())
        .await
        .unwrap();
    let result = timeout(Duration::from_secs(5), raw.server)
        .await
        .expect("server waited for the body")
        .unwrap();
    assert!(matches!(
        result,
        Err(Error::Protocol(ProtocolError::FrameTooLarge {
            max: 4096,
            ..
        }))
    ));
}

#[tokio::test]
async fn unknown_request_types_get_op_unsupported_and_the_connection_goes_on() {
    let mut raw = Raw::start(Arc::new(MemoryFs::new()), ServerConfig::default()).await;
    raw.handshake().await;
    raw.request(41, Op::Unknown { ty: 99 }).await;
    assert_eq!(raw.status().await, (41, StatusCode::OpUnsupported));
    raw.request(
        42,
        Op::Extended {
            name: b("nonsense@example"),
            data: Bytes::new(),
        },
    )
    .await;
    assert_eq!(raw.status().await, (42, StatusCode::OpUnsupported));
    raw.request(43, Op::Stat { path: b("/") }).await;
    assert!(matches!(raw.reply().await, (43, Reply::Attrs(_))));
    raw.finish().await.unwrap();
}

#[tokio::test]
async fn invalid_handles_fail_without_harm() {
    let mut raw = Raw::start(Arc::new(MemoryFs::new()), ServerConfig::default()).await;
    raw.handshake().await;
    // Nothing was ever issued: empty, short, long and well-formed guesses.
    for (id, handle) in [
        b(""),
        b("zz"),
        Bytes::from_static(&[0; 8]),
        Bytes::from_static(&[0, 0, 0, 0, 0, 0, 0, 1]),
        Bytes::from(vec![7u8; 5000]),
    ]
    .into_iter()
    .enumerate()
    {
        let id = id as u32;
        raw.request(
            id,
            Op::Close {
                handle: handle.clone(),
            },
        )
        .await;
        assert_eq!(raw.status().await, (id, StatusCode::Failure));
        raw.request(
            id,
            Op::Read {
                handle: handle.clone(),
                offset: 0,
                len: 1,
            },
        )
        .await;
        assert_eq!(raw.status().await, (id, StatusCode::Failure));
        raw.request(id, Op::ReadDir { handle }).await;
        assert_eq!(raw.status().await, (id, StatusCode::Failure));
    }

    // A closed handle is invalid, and so is the wrong kind of handle.
    raw.request(
        10,
        Op::Open {
            path: b("/f"),
            flags: OpenFlags::WRITE | OpenFlags::CREATE,
            attrs: Attributes::default(),
        },
    )
    .await;
    let (_, Reply::Handle(file)) = raw.reply().await else {
        panic!("no handle");
    };
    raw.request(11, Op::OpenDir { path: b("/") }).await;
    let (_, Reply::Handle(dir)) = raw.reply().await else {
        panic!("no handle");
    };
    raw.request(
        12,
        Op::ReadDir {
            handle: file.clone(),
        },
    )
    .await;
    assert_eq!(raw.status().await, (12, StatusCode::Failure));
    raw.request(
        13,
        Op::Read {
            handle: dir.clone(),
            offset: 0,
            len: 1,
        },
    )
    .await;
    assert_eq!(raw.status().await, (13, StatusCode::Failure));
    raw.request(
        14,
        Op::Close {
            handle: file.clone(),
        },
    )
    .await;
    assert_eq!(raw.status().await, (14, StatusCode::Ok));
    raw.request(15, Op::Close { handle: file }).await;
    assert_eq!(raw.status().await, (15, StatusCode::Failure));
    raw.finish().await.unwrap();
}

#[tokio::test]
async fn a_malformed_request_gets_bad_message_and_the_connection_goes_on() {
    let mut raw = Raw::start(Arc::new(MemoryFs::new()), ServerConfig::default()).await;
    raw.handshake().await;
    // A READ cut off after its id.
    raw.writer
        .write_all(&[0, 0, 0, 5, 5, 0, 0, 0, 77])
        .await
        .unwrap();
    assert_eq!(raw.status().await, (77, StatusCode::BadMessage));
    // A STAT with trailing junk.
    raw.writer
        .write_all(&[0, 0, 0, 11, 17, 0, 0, 0, 78, 0, 0, 0, 1, b'/', 9])
        .await
        .unwrap();
    assert_eq!(raw.status().await, (78, StatusCode::BadMessage));
    raw.request(79, Op::Stat { path: b("/") }).await;
    assert!(matches!(raw.reply().await, (79, Reply::Attrs(_))));
    // No id to answer: the connection ends.
    raw.writer.write_all(&[0, 0, 0, 2, 5, 0]).await.unwrap();
    let result = timeout(Duration::from_secs(5), raw.server)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(Error::Protocol(ProtocolError::Decode(_)))
    ));
}

#[tokio::test]
async fn handshake_violations_end_the_connection() {
    // A request before INIT.
    let mut raw = Raw::start(Arc::new(MemoryFs::new()), ServerConfig::default()).await;
    raw.request(1, Op::Stat { path: b("/") }).await;
    let result = timeout(Duration::from_secs(5), raw.server)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(Error::Protocol(ProtocolError::UnexpectedHandshake))
    ));

    // A second INIT.
    let mut raw = Raw::start(Arc::new(MemoryFs::new()), ServerConfig::default()).await;
    raw.handshake().await;
    raw.send(&ClientPacket::Init(Hello {
        version: 3,
        extensions: vec![],
    }))
    .await;
    let result = timeout(Duration::from_secs(5), raw.server)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(Error::Protocol(ProtocolError::UnexpectedHandshake))
    ));

    // An old version.
    let mut raw = Raw::start(Arc::new(MemoryFs::new()), ServerConfig::default()).await;
    raw.send(&ClientPacket::Init(Hello {
        version: 2,
        extensions: vec![],
    }))
    .await;
    let result = timeout(Duration::from_secs(5), raw.server)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(Error::Protocol(ProtocolError::UnsupportedVersion(2)))
    ));

    // The stream ends before anything is sent, or in the middle of a packet.
    let raw = Raw::start(Arc::new(MemoryFs::new()), ServerConfig::default()).await;
    assert!(raw.finish().await.is_err());
    let mut raw = Raw::start(Arc::new(MemoryFs::new()), ServerConfig::default()).await;
    raw.handshake().await;
    raw.writer.write_all(&[0, 0, 0, 20, 17, 0]).await.unwrap();
    assert!(raw.finish().await.is_err());
}

/// A file system for probing scheduling: it blocks, panics and logs on
/// command and implements nothing else.
#[derive(Default)]
struct ScriptFs {
    gate: Notify,
    running: AtomicUsize,
    peak: AtomicUsize,
    log: Mutex<Vec<String>>,
    closed: AtomicUsize,
}

impl ScriptFs {
    fn log(&self, entry: impl Into<String>) {
        self.log.lock().unwrap().push(entry.into());
    }
}

impl Filesystem for ScriptFs {
    type File = ();
    type Dir = ();

    async fn open(&self, _: &[u8], _: OpenFlags, _: &Attributes) -> Result<(), FsError> {
        Ok(())
    }
    async fn close_file(&self, _: ()) -> Result<(), FsError> {
        self.closed.fetch_add(1, Ordering::SeqCst);
        self.log("close");
        Ok(())
    }
    async fn read(&self, _: &(), _: u64, _: u32) -> Result<Option<Bytes>, FsError> {
        Ok(None)
    }
    async fn write(&self, _: &(), _: u64, data: Bytes) -> Result<(), FsError> {
        if &data[..] == b"slow" {
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        self.log(format!("write {}", String::from_utf8_lossy(&data)));
        Ok(())
    }
    async fn fstat(&self, _: &()) -> Result<Attributes, FsError> {
        Err(FsError::unsupported())
    }
    async fn fsetstat(&self, _: &(), _: &Attributes) -> Result<(), FsError> {
        Err(FsError::unsupported())
    }
    async fn stat(&self, path: &[u8]) -> Result<Attributes, FsError> {
        if path == b"/panic" {
            panic!("file system bug");
        }
        if path == b"/gate" {
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            self.gate.notified().await;
            self.running.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(Attributes::default())
    }
    async fn lstat(&self, _: &[u8]) -> Result<Attributes, FsError> {
        Err(FsError::unsupported())
    }
    async fn setstat(&self, _: &[u8], _: &Attributes) -> Result<(), FsError> {
        Err(FsError::unsupported())
    }
    async fn open_dir(&self, _: &[u8]) -> Result<(), FsError> {
        Ok(())
    }
    async fn read_dir(&self, _: &()) -> Result<Option<Vec<NameEntry>>, FsError> {
        Ok(None)
    }
    async fn remove(&self, _: &[u8]) -> Result<(), FsError> {
        Err(FsError::unsupported())
    }
    async fn mkdir(&self, _: &[u8], _: &Attributes) -> Result<(), FsError> {
        Err(FsError::unsupported())
    }
    async fn rmdir(&self, _: &[u8]) -> Result<(), FsError> {
        Err(FsError::unsupported())
    }
    async fn realpath(&self, _: &[u8]) -> Result<Bytes, FsError> {
        Err(FsError::unsupported())
    }
    async fn rename(&self, _: &[u8], _: &[u8]) -> Result<(), FsError> {
        Err(FsError::unsupported())
    }
    async fn read_link(&self, _: &[u8]) -> Result<Bytes, FsError> {
        Err(FsError::unsupported())
    }
    async fn symlink(&self, _: &[u8], _: &[u8]) -> Result<(), FsError> {
        Err(FsError::unsupported())
    }
}

#[tokio::test]
async fn the_pending_request_limit_stops_the_server_reading() {
    let fs_ = Arc::new(ScriptFs::default());
    let mut raw = Raw::start(fs_.clone(), ServerConfig::default().with_max_pending(2)).await;
    raw.handshake().await;
    for id in 0..5 {
        raw.request(id, Op::Stat { path: b("/gate") }).await;
    }
    // Wait for the two that may run to start, however long the machine
    // takes; the limit is checked by the peak, which can only be too high.
    for _ in 0..500 {
        if fs_.running.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(fs_.running.load(Ordering::SeqCst), 2, "only two may run");
    // Each release lets exactly one more request in.
    for done in 0..5 {
        fs_.gate.notify_one();
        let (_, status) = match raw.reply().await {
            (id, Reply::Attrs(_)) => (id, StatusCode::Ok),
            other => panic!("{other:?}"),
        };
        assert_eq!(status, StatusCode::Ok);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(fs_.running.load(Ordering::SeqCst) <= 2, "after {done}");
    }
    assert_eq!(fs_.peak.load(Ordering::SeqCst), 2);
    raw.finish().await.unwrap();
}

#[tokio::test]
async fn requests_on_one_handle_run_in_order_and_others_do_not_wait() {
    let fs_ = Arc::new(ScriptFs::default());
    let mut raw = Raw::start(fs_.clone(), ServerConfig::default()).await;
    raw.handshake().await;
    raw.request(
        1,
        Op::Open {
            path: b("/f"),
            flags: OpenFlags::WRITE,
            attrs: Attributes::default(),
        },
    )
    .await;
    let (_, Reply::Handle(h)) = raw.reply().await else {
        panic!("no handle");
    };
    // A slow write, a fast write and a close, sent back to back, with a
    // request on another path in between.
    let write = |data: &str| Op::Write {
        handle: h.clone(),
        offset: 0,
        data: b(data),
    };
    raw.request(2, write("slow")).await;
    raw.request(3, write("fast")).await;
    raw.request(4, Op::Stat { path: b("/x") }).await;
    raw.request(5, Op::Close { handle: h.clone() }).await;
    // The stat does not wait for the slow write...
    assert!(matches!(raw.reply().await, (4, Reply::Attrs(_))));
    // ...but everything on the handle finishes in request order.
    for want in [2, 3, 5] {
        assert_eq!(raw.status().await, (want, StatusCode::Ok));
    }
    assert_eq!(
        *fs_.log.lock().unwrap(),
        ["write slow", "write fast", "close"]
    );
    raw.finish().await.unwrap();
}

#[tokio::test]
async fn a_panicking_request_fails_alone() {
    let fs_ = Arc::new(ScriptFs::default());
    let mut raw = Raw::start(fs_, ServerConfig::default()).await;
    raw.handshake().await;
    raw.request(1, Op::Stat { path: b("/panic") }).await;
    assert_eq!(raw.status().await, (1, StatusCode::Failure));
    raw.request(2, Op::Stat { path: b("/fine") }).await;
    assert!(matches!(raw.reply().await, (2, Reply::Attrs(_))));
    raw.finish().await.unwrap();
}

#[tokio::test]
async fn handles_left_open_are_closed_when_the_client_leaves() {
    let fs_ = Arc::new(ScriptFs::default());
    let mut raw = Raw::start(fs_.clone(), ServerConfig::default()).await;
    raw.handshake().await;
    for id in 0..3 {
        raw.request(
            id,
            Op::Open {
                path: b("/f"),
                flags: OpenFlags::READ,
                attrs: Attributes::default(),
            },
        )
        .await;
        raw.reply().await;
    }
    raw.finish().await.unwrap();
    assert_eq!(fs_.closed.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn directory_replies_are_bounded_and_carry_long_names() {
    let fs_ = Arc::new(MemoryFs::new());
    for i in 0..10 {
        fs_.insert_file(format!("/file{i}"), b"x").unwrap();
    }
    let config = ServerConfig::default().with_max_dir_entries(3);
    let (s, _server) = session_with(fs_, config).await;
    let dir = s.opendir("/").await.unwrap();
    let mut batches = Vec::new();
    while let Some(batch) = s.readdir(&dir).await.unwrap() {
        batches.push(batch);
    }
    assert!(batches.iter().all(|b| b.len() <= 3));
    let all: Vec<_> = batches.into_iter().flatten().collect();
    // ".", ".." and ten files, none lost or repeated by the batching.
    assert_eq!(all.len(), 12);
    let mut names: Vec<_> = all.iter().map(|e| e.name.clone()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 12);
    let entry = all.iter().find(|e| e.name == "file3").unwrap();
    let long = String::from_utf8(entry.long_name.to_vec()).unwrap();
    assert!(long.starts_with("-rw-r--r--"), "{long}");
    assert!(long.ends_with(" file3"), "{long}");
}

#[tokio::test]
async fn dropping_the_server_ends_the_client_session() {
    let fs_ = Arc::new(MemoryFs::new());
    let (s, server) = session_with(fs_, ServerConfig::default()).await;
    s.stat("/").await.unwrap();
    server.abort();
    let result = timeout(Duration::from_secs(5), s.stat("/")).await.unwrap();
    assert!(matches!(result, Err(Error::Disconnected | Error::Io(_))));
}

#[tokio::test]
async fn many_clients_share_one_file_system() {
    let fs_ = Arc::new(MemoryFs::new());
    let mut tasks = Vec::new();
    for i in 0..4u64 {
        let fs_ = fs_.clone();
        tasks.push(tokio::spawn(async move {
            let (s, _server) = session_with(fs_, ServerConfig::default()).await;
            let data = random_bytes(200_000, i);
            fs::write(&s, format!("/c{i}"), data.clone()).await.unwrap();
            assert_eq!(fs::read(&s, format!("/c{i}")).await.unwrap(), data);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
}

#[tokio::test]
async fn time_limits_end_silent_connections() {
    // No handshake within the limit.
    let raw = Raw::start(
        Arc::new(MemoryFs::new()),
        ServerConfig::default().with_handshake_timeout(Some(Duration::from_millis(50))),
    )
    .await;
    let result = timeout(Duration::from_secs(5), raw.server)
        .await
        .expect("the server kept waiting for a handshake")
        .unwrap();
    match result {
        Err(Error::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::TimedOut),
        other => panic!("{other:?}"),
    }

    // Idle after the handshake: ended. With no limit (the default) it would
    // wait for ever, which the next part relies on.
    let mut raw = Raw::start(
        Arc::new(MemoryFs::new()),
        ServerConfig::default().with_idle_timeout(Some(Duration::from_millis(100))),
    )
    .await;
    raw.handshake().await;
    raw.request(1, Op::Stat { path: b("/") }).await;
    assert!(matches!(raw.reply().await, (1, Reply::Attrs(_))));
    let result = timeout(Duration::from_secs(5), raw.server)
        .await
        .expect("the idle connection was kept")
        .unwrap();
    match result {
        Err(Error::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::TimedOut),
        other => panic!("{other:?}"),
    }

    // A request that takes longer than the idle limit is not idleness: the
    // connection stays up while it is being served.
    let fs_ = Arc::new(ScriptFs::default());
    let mut raw = Raw::start(
        fs_.clone(),
        ServerConfig::default().with_idle_timeout(Some(Duration::from_millis(50))),
    )
    .await;
    raw.handshake().await;
    raw.request(1, Op::Stat { path: b("/gate") }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !raw.server.is_finished(),
        "ended while a request was running"
    );
    fs_.gate.notify_one();
    assert!(matches!(raw.reply().await, (1, Reply::Attrs(_))));
}
