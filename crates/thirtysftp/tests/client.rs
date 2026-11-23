//! The client against a scripted in-memory server.
#![cfg(feature = "client")]

mod common;

use std::time::Duration;

use common::{Scripted as Server, scripted as start, status_reply};

use bytes::{Bytes, BytesMut};
use thirtysftp::client::{Config, File, Handle, Session};
use thirtysftp::codec::FrameReader;
use thirtysftp::wire::{
    Attributes, Encode, Extension, Hello, NameEntry, Op, OpenFlags, Reply, Request, Response,
    ServerPacket, StatusCode,
};
use thirtysftp::{Error, ProtocolError};
use tokio::io::{AsyncWriteExt, duplex, split};
use tokio::time::timeout;

fn status(code: StatusCode) -> Reply {
    status_reply(code)
}

fn b(s: &str) -> Bytes {
    Bytes::copy_from_slice(s.as_bytes())
}

async fn session() -> (Session, Server) {
    start(Config::default(), vec![]).await
}

/// Runs `call` against the server, which answers its single request with
/// `reply` after checking it is `expected`.
async fn exchange<T>(
    call: impl std::future::Future<Output = T>,
    server: &mut Server,
    expected: Op,
    reply: Reply,
) -> T {
    let serve = async {
        let request = server.request().await;
        assert_eq!(request.op, expected);
        server.reply(request.id, reply).await;
    };
    let (out, ()) = tokio::join!(call, serve);
    out
}

#[tokio::test]
async fn handshake_exposes_extensions() {
    let (session, _server) = start(
        Config::default(),
        vec![Extension {
            name: b("posix-rename@openssh.com"),
            data: b("1"),
        }],
    )
    .await;
    assert_eq!(session.extensions().len(), 1);
    assert_eq!(session.extension("posix-rename@openssh.com"), Some(&b("1")));
    assert_eq!(session.extension("hardlink@openssh.com"), None);
}

async fn handshake_with(version_packet: Vec<u8>) -> Result<Session, Error> {
    let (client, server) = duplex(4096);
    let (cr, cw) = split(client);
    let (sr, mut sw) = split(server);
    let connect = tokio::spawn(Session::connect(cr, cw, Config::default()));
    let mut reader = FrameReader::new(sr, 1 << 16);
    reader.read_frame().await.unwrap().unwrap();
    sw.write_all(&version_packet).await.unwrap();
    // Both halves must go for the client to see the end of the stream.
    drop(sw);
    drop(reader);
    connect.await.unwrap()
}

fn frame_of(packet: ServerPacket) -> Vec<u8> {
    let mut out = BytesMut::new();
    packet.encode_frame(&mut out).unwrap();
    out.to_vec()
}

#[tokio::test]
async fn handshake_rejects_other_versions() {
    for version in [0, 2, 4, 6] {
        let packet = frame_of(ServerPacket::Version(Hello {
            version,
            extensions: vec![],
        }));
        assert!(
            matches!(
                handshake_with(packet).await,
                Err(Error::Protocol(ProtocolError::UnsupportedVersion(v))) if v == version
            ),
            "version {version}"
        );
    }
}

#[tokio::test]
async fn handshake_rejects_garbage_and_early_close() {
    let garbage = vec![0, 0, 0, 3, 2, 0, 0];
    assert!(matches!(
        handshake_with(garbage).await,
        Err(Error::Protocol(ProtocolError::Decode(_)))
    ));
    assert!(matches!(
        handshake_with(vec![]).await,
        Err(Error::Disconnected)
    ));
    let reply = frame_of(ServerPacket::Response(Response {
        id: 0,
        reply: status(StatusCode::Ok),
    }));
    assert!(matches!(
        handshake_with(reply).await,
        Err(Error::Protocol(ProtocolError::UnexpectedReply(_)))
    ));
}

#[tokio::test]
async fn every_operation_sends_its_request_and_reads_its_reply() {
    let (s, mut srv) = session().await;
    let h = Handle::from_bytes(b("H"));
    let attrs = Attributes {
        size: Some(3),
        ..Attributes::default()
    };

    let opened = exchange(
        s.open("/f", OpenFlags::READ, Attributes::default()),
        &mut srv,
        Op::Open {
            path: b("/f"),
            flags: OpenFlags::READ,
            attrs: Attributes::default(),
        },
        Reply::Handle(b("H")),
    )
    .await
    .unwrap();
    assert_eq!(opened, h);

    let data = exchange(
        s.read(&h, 1 << 33, 10),
        &mut srv,
        Op::Read {
            handle: b("H"),
            offset: 1 << 33,
            len: 10,
        },
        Reply::Data(b("abc")),
    )
    .await
    .unwrap();
    assert_eq!(data, Some(b("abc")));

    exchange(
        s.write(&h, 5, b("xyz")),
        &mut srv,
        Op::Write {
            handle: b("H"),
            offset: 5,
            data: b("xyz"),
        },
        status(StatusCode::Ok),
    )
    .await
    .unwrap();

    for (name, op, reply) in [
        (
            "stat",
            Op::Stat { path: b("/p") },
            Reply::Attrs(attrs.clone()),
        ),
        (
            "lstat",
            Op::Lstat { path: b("/p") },
            Reply::Attrs(attrs.clone()),
        ),
    ] {
        let got = exchange(
            async {
                if name == "stat" {
                    s.stat("/p").await
                } else {
                    s.lstat("/p").await
                }
            },
            &mut srv,
            op,
            reply,
        )
        .await
        .unwrap();
        assert_eq!(got, attrs);
    }

    let got = exchange(
        s.fstat(&h),
        &mut srv,
        Op::Fstat { handle: b("H") },
        Reply::Attrs(attrs.clone()),
    )
    .await
    .unwrap();
    assert_eq!(got, attrs);

    exchange(
        s.setstat("/p", attrs.clone()),
        &mut srv,
        Op::SetStat {
            path: b("/p"),
            attrs: attrs.clone(),
        },
        status(StatusCode::Ok),
    )
    .await
    .unwrap();
    exchange(
        s.fsetstat(&h, attrs.clone()),
        &mut srv,
        Op::FsetStat {
            handle: b("H"),
            attrs: attrs.clone(),
        },
        status(StatusCode::Ok),
    )
    .await
    .unwrap();

    let dir = exchange(
        s.opendir("/d"),
        &mut srv,
        Op::OpenDir { path: b("/d") },
        Reply::Handle(b("D")),
    )
    .await
    .unwrap();
    let entry = NameEntry {
        name: b("a"),
        long_name: b("a"),
        attrs: Attributes::default(),
    };
    let batch = exchange(
        s.readdir(&dir),
        &mut srv,
        Op::ReadDir { handle: b("D") },
        Reply::Name(vec![entry.clone()]),
    )
    .await
    .unwrap();
    assert_eq!(batch, Some(vec![entry]));
    let end = exchange(
        s.readdir(&dir),
        &mut srv,
        Op::ReadDir { handle: b("D") },
        status(StatusCode::Eof),
    )
    .await
    .unwrap();
    assert_eq!(end, None);

    exchange(
        s.remove("/r"),
        &mut srv,
        Op::Remove { path: b("/r") },
        status(StatusCode::Ok),
    )
    .await
    .unwrap();
    exchange(
        s.mkdir("/m", Attributes::default()),
        &mut srv,
        Op::MkDir {
            path: b("/m"),
            attrs: Attributes::default(),
        },
        status(StatusCode::Ok),
    )
    .await
    .unwrap();
    exchange(
        s.rmdir("/m"),
        &mut srv,
        Op::RmDir { path: b("/m") },
        status(StatusCode::Ok),
    )
    .await
    .unwrap();
    exchange(
        s.rename("/a", "/b"),
        &mut srv,
        Op::Rename {
            from: b("/a"),
            to: b("/b"),
        },
        status(StatusCode::Ok),
    )
    .await
    .unwrap();
    exchange(
        s.symlink("target", "link"),
        &mut srv,
        Op::Symlink {
            target: b("target"),
            link: b("link"),
        },
        status(StatusCode::Ok),
    )
    .await
    .unwrap();

    let one = |name: &str| {
        Reply::Name(vec![NameEntry {
            name: b(name),
            long_name: Bytes::new(),
            attrs: Attributes::default(),
        }])
    };
    let real = exchange(
        s.realpath("."),
        &mut srv,
        Op::RealPath { path: b(".") },
        one("/home"),
    )
    .await
    .unwrap();
    assert_eq!(real, b("/home"));
    let link = exchange(
        s.readlink("/l"),
        &mut srv,
        Op::ReadLink { path: b("/l") },
        one("t"),
    )
    .await
    .unwrap();
    assert_eq!(link, b("t"));

    let ext = exchange(
        s.extended("x@example", b("in")),
        &mut srv,
        Op::Extended {
            name: b("x@example"),
            data: b("in"),
        },
        Reply::Extended(b("out")),
    )
    .await
    .unwrap();
    assert_eq!(ext, b("out"));

    exchange(
        s.close_handle(&h),
        &mut srv,
        Op::Close { handle: b("H") },
        status(StatusCode::Ok),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn status_errors_carry_the_code() {
    let (s, mut srv) = session().await;
    let err = exchange(
        s.stat("/missing"),
        &mut srv,
        Op::Stat {
            path: b("/missing"),
        },
        status(StatusCode::NoSuchFile),
    )
    .await
    .unwrap_err();
    assert_eq!(err.status_code(), Some(StatusCode::NoSuchFile));
    let io: std::io::Error = err.into();
    assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
    // The session is still usable after an operation-level error.
    exchange(
        s.remove("/x"),
        &mut srv,
        Op::Remove { path: b("/x") },
        status(StatusCode::Ok),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn replies_may_arrive_in_any_order() {
    let (s, mut srv) = session().await;
    let h = Handle::from_bytes(b("H"));
    let calls = async {
        tokio::join!(
            s.read(&h, 0, 16),
            s.read(&h, 100, 16),
            s.read(&h, 200, 16),
            s.read(&h, 300, 16),
        )
    };
    let serve = async {
        let mut requests = Vec::new();
        for _ in 0..4 {
            requests.push(srv.request().await);
        }
        // Answer last to first. Each reply names its offset.
        for r in requests.into_iter().rev() {
            let Op::Read { offset, .. } = r.op else {
                panic!("not a read")
            };
            srv.reply(r.id, Reply::Data(Bytes::from(offset.to_string())))
                .await;
        }
    };
    let ((a, b_, c, d), ()) = tokio::join!(calls, serve);
    assert_eq!(a.unwrap(), Some(b("0")));
    assert_eq!(b_.unwrap(), Some(b("100")));
    assert_eq!(c.unwrap(), Some(b("200")));
    assert_eq!(d.unwrap(), Some(b("300")));
}

#[tokio::test]
async fn an_abandoned_request_keeps_its_slot_and_id_until_the_reply() {
    let config = Config::default().with_max_in_flight(1);
    let (s, mut srv) = start(config, vec![]).await;

    // Send a request and give up waiting for it.
    let abandoned = timeout(Duration::from_millis(20), s.stat("/old")).await;
    assert!(abandoned.is_err());
    let old = srv.request().await;

    // With one slot, the next request cannot even be sent yet.
    let s2 = s.clone();
    let next = tokio::spawn(async move { s2.stat("/new").await });
    assert!(
        timeout(Duration::from_millis(50), srv.request())
            .await
            .is_err(),
        "second request sent while the first still holds the only slot"
    );

    // The late reply frees the slot without reaching the new request.
    srv.reply(
        old.id,
        Reply::Attrs(Attributes {
            size: Some(111),
            ..Attributes::default()
        }),
    )
    .await;
    let new = srv.request().await;
    assert_eq!(new.op, Op::Stat { path: b("/new") });
    srv.reply(
        new.id,
        Reply::Attrs(Attributes {
            size: Some(222),
            ..Attributes::default()
        }),
    )
    .await;
    assert_eq!(next.await.unwrap().unwrap().size, Some(222));
}

#[tokio::test]
async fn cancelling_while_waiting_for_a_slot_sends_nothing() {
    let config = Config::default().with_max_in_flight(1);
    let (s, mut srv) = start(config, vec![]).await;
    let holder = {
        let s = s.clone();
        tokio::spawn(async move { s.stat("/a").await })
    };
    let first = srv.request().await;
    // Queued behind the slot, then cancelled.
    assert!(
        timeout(Duration::from_millis(20), s.stat("/cancelled"))
            .await
            .is_err()
    );
    srv.reply(first.id, Reply::Attrs(Attributes::default()))
        .await;
    holder.await.unwrap().unwrap();
    let after = {
        let s = s.clone();
        tokio::spawn(async move { s.stat("/after").await })
    };
    // The cancelled request never reaches the server.
    assert_eq!(srv.request().await.op, Op::Stat { path: b("/after") });
    after.abort();
}

#[tokio::test]
async fn the_in_flight_limit_holds_requests_back() {
    let config = Config::default().with_max_in_flight(2);
    let (s, mut srv) = start(config, vec![]).await;
    let mut tasks = Vec::new();
    for i in 0..5 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move { s.stat(format!("/{i}")).await }));
    }
    let a = srv.request().await;
    let b_ = srv.request().await;
    assert!(
        timeout(Duration::from_millis(50), srv.request())
            .await
            .is_err(),
        "a third request went out"
    );
    srv.reply(a.id, Reply::Attrs(Attributes::default())).await;
    // One slot opened, so exactly one more request may follow.
    let c = srv.request().await;
    assert!(
        timeout(Duration::from_millis(50), srv.request())
            .await
            .is_err()
    );
    for id in [b_.id, c.id] {
        srv.reply(id, Reply::Attrs(Attributes::default())).await;
    }
    for _ in 0..2 {
        let r = srv.request().await;
        srv.reply(r.id, Reply::Attrs(Attributes::default())).await;
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn losing_the_connection_wakes_every_waiter() {
    let (s, mut srv) = session().await;
    let tasks: Vec<_> = (0..4)
        .map(|i| {
            let s = s.clone();
            tokio::spawn(async move { s.stat(format!("/{i}")).await })
        })
        .collect();
    for _ in 0..4 {
        srv.request().await;
    }
    drop(srv);
    for t in tasks {
        let result = timeout(Duration::from_secs(5), t).await.unwrap().unwrap();
        assert!(matches!(result, Err(Error::Disconnected)), "{result:?}");
    }
    // Later requests fail at once instead of waiting.
    let later = timeout(Duration::from_secs(5), s.stat("/later"))
        .await
        .unwrap();
    assert!(matches!(later, Err(Error::Disconnected)));
}

#[tokio::test]
async fn a_reply_to_an_unknown_id_ends_the_session() {
    let (s, mut srv) = session().await;
    let waiter = {
        let s = s.clone();
        tokio::spawn(async move { s.stat("/a").await })
    };
    let real = srv.request().await;
    srv.reply(
        real.id.wrapping_add(1000),
        Reply::Attrs(Attributes::default()),
    )
    .await;
    let result = timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(Error::Protocol(ProtocolError::UnknownRequestId(_)))
    ));
    assert!(s.stat("/b").await.is_err());
}

#[tokio::test]
async fn a_duplicate_reply_ends_the_session() {
    let (s, mut srv) = session().await;
    let call = s.remove("/a");
    let serve = async {
        let r = srv.request().await;
        srv.status(r.id, StatusCode::Ok).await;
        srv.status(r.id, StatusCode::Ok).await;
    };
    let (result, ()) = tokio::join!(call, serve);
    result.unwrap();
    let again = timeout(Duration::from_secs(5), s.remove("/b"))
        .await
        .unwrap();
    assert!(matches!(
        again,
        Err(Error::Protocol(ProtocolError::UnknownRequestId(_)))
    ));
}

#[tokio::test]
async fn a_garbled_packet_ends_the_session() {
    let (s, mut srv) = session().await;
    let waiter = {
        let s = s.clone();
        tokio::spawn(async move { s.stat("/a").await })
    };
    srv.request().await;
    // A STATUS reply cut off in the middle of its code.
    srv.writer
        .write_all(&[0, 0, 0, 7, 101, 0, 0, 0, 1, 0, 0])
        .await
        .unwrap();
    let result = timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(Error::Protocol(ProtocolError::Decode(_)))
    ));
}

#[tokio::test]
async fn an_oversized_reply_ends_the_session() {
    let config = Config::default().with_max_frame(64);
    let (s, mut srv) = start(config, vec![]).await;
    let waiter = {
        let s = s.clone();
        tokio::spawn(async move { s.stat("/a").await })
    };
    srv.request().await;
    // Only the length field arrives; the client must not wait for the body.
    srv.writer
        .write_all(&1_000_000u32.to_be_bytes())
        .await
        .unwrap();
    let result = timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(Error::Protocol(ProtocolError::FrameTooLarge { .. }))
    ));
}

#[tokio::test]
async fn mismatched_replies_are_protocol_errors() {
    let (s, mut srv) = session().await;
    // Success where a handle was due.
    let err = exchange(
        s.opendir("/d"),
        &mut srv,
        Op::OpenDir { path: b("/d") },
        status(StatusCode::Ok),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Protocol(ProtocolError::UnexpectedReply(_))
    ));
    // Attributes where success was due.
    let err = exchange(
        s.remove("/x"),
        &mut srv,
        Op::Remove { path: b("/x") },
        Reply::Attrs(Attributes::default()),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Protocol(ProtocolError::UnexpectedReply(_))
    ));
    // Two names where one was due.
    let name = NameEntry {
        name: b("a"),
        long_name: Bytes::new(),
        attrs: Attributes::default(),
    };
    let err = exchange(
        s.realpath("."),
        &mut srv,
        Op::RealPath { path: b(".") },
        Reply::Name(vec![name.clone(), name]),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Protocol(ProtocolError::UnexpectedReply(_))
    ));
    // A reply type nobody knows.
    let err = exchange(
        s.remove("/x"),
        &mut srv,
        Op::Remove { path: b("/x") },
        Reply::Unknown { ty: 150 },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Protocol(ProtocolError::UnexpectedReply(_))
    ));
}

#[tokio::test]
async fn a_read_reply_longer_than_the_request_is_rejected() {
    let (s, mut srv) = session().await;
    let h = Handle::from_bytes(b("H"));
    let err = exchange(
        s.read(&h, 0, 2),
        &mut srv,
        Op::Read {
            handle: b("H"),
            offset: 0,
            len: 2,
        },
        Reply::Data(b("abc")),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Protocol(ProtocolError::OversizedData)));
}

#[tokio::test]
async fn a_short_read_is_returned_as_is() {
    let (s, mut srv) = session().await;
    let h = Handle::from_bytes(b("H"));
    let got = exchange(
        s.read(&h, 0, 100),
        &mut srv,
        Op::Read {
            handle: b("H"),
            offset: 0,
            len: 100,
        },
        Reply::Data(b("ab")),
    )
    .await
    .unwrap();
    assert_eq!(got, Some(b("ab")));
}

#[tokio::test]
async fn close_fails_waiters_and_shuts_the_stream() {
    let (s, mut srv) = session().await;
    let waiter = {
        let s = s.clone();
        tokio::spawn(async move { s.stat("/a").await })
    };
    srv.request().await;
    s.close();
    let result = timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(Error::Closed)));
    assert!(matches!(s.stat("/b").await, Err(Error::Closed)));
    // The server sees the end of the stream.
    let end = timeout(Duration::from_secs(5), srv.reader.read_frame())
        .await
        .unwrap();
    assert!(end.unwrap().is_none());
}

#[tokio::test]
async fn dropping_every_handle_closes_the_stream() {
    let (s, mut srv) = session().await;
    let clone = s.clone();
    drop(s);
    // One handle is still alive.
    assert!(
        timeout(Duration::from_millis(50), srv.reader.read_frame())
            .await
            .is_err()
    );
    drop(clone);
    let end = timeout(Duration::from_secs(5), srv.reader.read_frame())
        .await
        .unwrap();
    assert!(end.unwrap().is_none());
}

#[tokio::test]
async fn slots_are_released_after_every_reply() {
    let config = Config::default().with_max_in_flight(2);
    let (s, mut srv) = start(config, vec![]).await;
    for i in 0..50 {
        let call = s.remove(format!("/{i}"));
        let serve = async {
            let r = srv.request().await;
            srv.status(r.id, StatusCode::Ok).await;
        };
        let (result, ()) = tokio::join!(call, serve);
        result.unwrap();
    }
}

/// The byte a test file holds at `offset`.
fn byte_at(offset: u64) -> u8 {
    (offset % 251) as u8
}

fn file_bytes(offset: u64, len: usize) -> Bytes {
    (0..len as u64).map(|i| byte_at(offset + i)).collect()
}

#[tokio::test]
async fn reading_a_file_as_a_stream_reads_ahead_and_recovers_from_short_replies() {
    use tokio::io::AsyncReadExt;
    const CHUNK: usize = 32 * 1024;
    let (s, mut srv) = session().await;

    let reader = tokio::spawn(async move {
        let mut file = File::open(&s, "/f").await.unwrap();
        let mut got = Vec::new();
        let mut block = vec![0u8; CHUNK];
        for _ in 0..3 {
            file.read_exact(&mut block).await.unwrap();
            got.extend_from_slice(&block);
        }
        let n = file.read(&mut block).await.unwrap();
        got.extend_from_slice(&block[..n]);
        let end = file.read(&mut block).await.unwrap();
        (got, n, end)
    });

    let open = srv.request().await;
    srv.reply(open.id, Reply::Handle(b("H"))).await;
    let offset_of = |r: &Request| match &r.op {
        Op::Read { offset, len, .. } => {
            assert_eq!(*len as usize, CHUNK);
            *offset
        }
        other => panic!("not a read: {other:?}"),
    };
    let full = |offset: u64| Reply::Data(file_bytes(offset, CHUNK));

    // One request at first.
    let a = srv.request().await;
    assert_eq!(offset_of(&a), 0);
    srv.reply(a.id, full(0)).await;
    // After a full reply, two are sent before either is answered. They are
    // answered out of order here.
    let (b_, c) = (srv.request().await, srv.request().await);
    assert_eq!(offset_of(&b_), CHUNK as u64);
    assert_eq!(offset_of(&c), 2 * CHUNK as u64);
    srv.reply(c.id, full(2 * CHUNK as u64)).await;
    srv.reply(b_.id, full(CHUNK as u64)).await;
    // The window keeps doubling: three more, then five more, all unanswered.
    let mut ahead = Vec::new();
    for i in 3..11u64 {
        let r = srv.request().await;
        assert_eq!(offset_of(&r), i * CHUNK as u64);
        ahead.push(r);
    }
    // A short reply: 100 bytes where a chunk was asked for.
    let short_at = 3 * CHUNK as u64;
    srv.reply(ahead[0].id, Reply::Data(file_bytes(short_at, 100)))
        .await;
    // The reads sent after it are abandoned, and reading resumes exactly
    // where the short reply ended.
    let resumed = srv.request().await;
    assert_eq!(offset_of(&resumed), short_at + 100);
    srv.status(resumed.id, StatusCode::Eof).await;

    let (got, n, end) = reader.await.unwrap();
    assert_eq!(n, 100);
    assert_eq!(end, 0);
    assert_eq!(got.len(), 3 * CHUNK + 100);
    assert!(got.iter().enumerate().all(|(i, &v)| v == byte_at(i as u64)));
}

#[tokio::test]
async fn closing_abandons_a_write_the_peer_is_not_reading() {
    use tokio::io::AsyncReadExt;
    // A pipe that holds 1 KiB, and a server that stops reading after the
    // handshake.
    let (client, server) = duplex(1024);
    let (cr, cw) = split(client);
    let (mut sr, mut sw) = split(server);
    let connect = tokio::spawn(Session::connect(cr, cw, Config::default()));
    let mut init = [0u8; 9];
    sr.read_exact(&mut init).await.unwrap();
    sw.write_all(&frame_of(ServerPacket::Version(Hello {
        version: 3,
        extensions: vec![],
    })))
    .await
    .unwrap();
    let s = connect.await.unwrap().unwrap();

    // A megabyte to write: the writer blocks once the pipe is full.
    let stuck = {
        let s = s.clone();
        tokio::spawn(async move {
            s.write(
                &Handle::from_bytes(b("H")),
                0,
                Bytes::from(vec![7u8; 1 << 20]),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    s.close();
    assert!(matches!(stuck.await.unwrap(), Err(Error::Closed)));

    // The stream is released: the server reads what was already in the pipe
    // and then the end, not the megabyte the writer was holding.
    let mut rest = Vec::new();
    timeout(Duration::from_secs(5), sr.read_to_end(&mut rest))
        .await
        .expect("the writer still holds the stream")
        .unwrap();
    assert!(rest.len() < 1 << 20, "{} bytes arrived", rest.len());
}
