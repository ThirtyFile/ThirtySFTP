//! The typed OpenSSH extensions against a scripted server: the bytes sent,
//! the replies parsed, and the refusal to send what the server never
//! announced.
#![cfg(feature = "client")]

mod common;

use std::time::Duration;

use common::{Scripted as Server, scripted};

use bytes::{BufMut, Bytes, BytesMut};
use thirtysftp::client::{Config, Handle, Session, StatVfs, extensions as ext};
use thirtysftp::codec::FrameReader;
use thirtysftp::wire::{
    Attributes, Encode, Extension, Hello, NameEntry, Op, Reply, Response, ServerPacket, StatusCode,
};
use thirtysftp::{Error, ProtocolError};
use tokio::io::{AsyncWriteExt, duplex, split};
use tokio::time::timeout;

/// Expects one extended request named `name` with exactly `data`, and
/// answers it.
async fn expect(server: &mut Server, name: &str, data: &[u8], reply: Reply) {
    let r = server.request().await;
    let Op::Extended {
        name: got,
        data: got_data,
    } = r.op
    else {
        panic!("not extended: {:?}", r.op);
    };
    assert_eq!(got, name.as_bytes());
    assert_eq!(&got_data[..], data, "request data of {name}");
    server.reply(r.id, reply).await;
}

fn s(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = Vec::new();
    for p in parts {
        v.extend((p.len() as u32).to_be_bytes());
        v.extend_from_slice(p);
    }
    v
}

fn u64s(values: &[u64]) -> Bytes {
    let mut out = BytesMut::new();
    for v in values {
        out.put_u64(*v);
    }
    out.freeze()
}

fn ok() -> Reply {
    Reply::Status {
        code: StatusCode::Ok,
        message: Bytes::new(),
        language: Bytes::new(),
    }
}

fn name(n: &str) -> Reply {
    Reply::Name(vec![NameEntry {
        name: Bytes::copy_from_slice(n.as_bytes()),
        long_name: Bytes::new(),
        attrs: Attributes::default(),
    }])
}

async fn start(announce: &[&str]) -> (Session, Server) {
    let extensions = announce
        .iter()
        .filter(|n| **n != ext::LIMITS)
        .map(|n| Extension {
            name: Bytes::copy_from_slice(n.as_bytes()),
            data: Bytes::from_static(b"1"),
        })
        .collect();
    scripted(Config::default(), extensions).await
}

const ALL: &[&str] = &[
    ext::POSIX_RENAME,
    ext::STATVFS,
    ext::FSTATVFS,
    ext::HARDLINK,
    ext::FSYNC,
    ext::LSETSTAT,
    ext::EXPAND_PATH,
    ext::COPY_DATA,
    ext::HOME_DIRECTORY,
    ext::USERS_GROUPS_BY_ID,
];

/// Runs `call` while the server handles one extended request.
async fn run<T>(
    call: impl std::future::Future<Output = T>,
    server: &mut Server,
    name: &str,
    data: &[u8],
    reply: Reply,
) -> T {
    let (out, ()) = tokio::join!(call, expect(server, name, data, reply));
    out
}

#[tokio::test]
async fn requests_that_answer_with_a_status() {
    let (session, mut server) = start(ALL).await;
    let h = Handle::from_bytes(Bytes::from_static(b"H"));
    let g = Handle::from_bytes(Bytes::from_static(b"G"));

    run(
        session.posix_rename("/a", "/b"),
        &mut server,
        ext::POSIX_RENAME,
        &s(&[b"/a", b"/b"]),
        ok(),
    )
    .await
    .unwrap();
    run(
        session.hardlink("/a", "/link"),
        &mut server,
        ext::HARDLINK,
        &s(&[b"/a", b"/link"]),
        ok(),
    )
    .await
    .unwrap();
    run(
        session.fsync(&h),
        &mut server,
        ext::FSYNC,
        &s(&[b"H"]),
        ok(),
    )
    .await
    .unwrap();

    let mut expected = s(&[b"/p"]);
    expected.extend([0, 0, 0, 4, 0, 0, 0x01, 0xed]); // permissions flag, 0o755
    run(
        session.lsetstat(
            "/p",
            &Attributes {
                permissions: Some(0o755),
                ..Attributes::default()
            },
        ),
        &mut server,
        ext::LSETSTAT,
        &expected,
        ok(),
    )
    .await
    .unwrap();

    let mut copy = s(&[b"H"]);
    copy.extend(7u64.to_be_bytes());
    copy.extend(100u64.to_be_bytes());
    copy.extend(s(&[b"G"]));
    copy.extend((1u64 << 40).to_be_bytes());
    run(
        session.copy_data(&h, 7, 100, &g, 1 << 40),
        &mut server,
        ext::COPY_DATA,
        &copy,
        ok(),
    )
    .await
    .unwrap();

    // A server failure comes back as its status code.
    let err = run(
        session.posix_rename("/a", "/b"),
        &mut server,
        ext::POSIX_RENAME,
        &s(&[b"/a", b"/b"]),
        Reply::Status {
            code: StatusCode::PermissionDenied,
            message: Bytes::from_static(b"no"),
            language: Bytes::new(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.status_code(), Some(StatusCode::PermissionDenied));
}

#[tokio::test]
async fn statvfs_reply_is_parsed() {
    let (session, mut server) = start(ALL).await;
    let h = Handle::from_bytes(Bytes::from_static(b"H"));
    let values = [4096, 4096, 1000, 400, 300, 50, 20, 10, 0xabcd, 1, 255];
    let expected = StatVfs {
        block_size: 4096,
        fragment_size: 4096,
        blocks: 1000,
        blocks_free: 400,
        blocks_available: 300,
        files: 50,
        files_free: 20,
        files_available: 10,
        id: 0xabcd,
        flags: StatVfs::READ_ONLY,
        name_max: 255,
    };
    let got = run(
        session.statvfs("/mnt"),
        &mut server,
        ext::STATVFS,
        &s(&[b"/mnt"]),
        Reply::Extended(u64s(&values)),
    )
    .await
    .unwrap();
    assert_eq!(got, expected);
    let got = run(
        session.fstatvfs(&h),
        &mut server,
        ext::FSTATVFS,
        &s(&[b"H"]),
        Reply::Extended(u64s(&values)),
    )
    .await
    .unwrap();
    assert_eq!(got, expected);

    // Too short and too long are both protocol errors.
    for bad in [u64s(&values[..10]), u64s(&[0; 12])] {
        let err = run(
            session.statvfs("/mnt"),
            &mut server,
            ext::STATVFS,
            &s(&[b"/mnt"]),
            Reply::Extended(bad),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::Protocol(ProtocolError::Decode(_))));
    }
}

#[tokio::test]
async fn name_replies() {
    let (session, mut server) = start(ALL).await;
    let got = run(
        session.expand_path("~/x"),
        &mut server,
        ext::EXPAND_PATH,
        &s(&[b"~/x"]),
        name("/home/u/x"),
    )
    .await
    .unwrap();
    assert_eq!(got, "/home/u/x");
    let got = run(
        session.home_directory("bob"),
        &mut server,
        ext::HOME_DIRECTORY,
        &s(&[b"bob"]),
        name("/home/bob"),
    )
    .await
    .unwrap();
    assert_eq!(got, "/home/bob");
    let err = run(
        session.expand_path("~"),
        &mut server,
        ext::EXPAND_PATH,
        &s(&[b"~"]),
        Reply::Name(vec![]),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Protocol(ProtocolError::UnexpectedReply(_))
    ));
}

#[tokio::test]
async fn users_and_groups_by_id() {
    let (session, mut server) = start(ALL).await;
    let mut uids = Vec::new();
    for id in [0u32, 1000] {
        uids.extend(id.to_be_bytes());
    }
    let request = s(&[&uids, &1000u32.to_be_bytes()]);
    let reply = Reply::Extended(Bytes::from(s(&[&s(&[b"root", b""]), &s(&[b"staff"])])));
    let (users, groups) = run(
        session.users_groups_by_id(&[0, 1000], &[1000]),
        &mut server,
        ext::USERS_GROUPS_BY_ID,
        &request,
        reply,
    )
    .await
    .unwrap();
    assert_eq!(users, [Bytes::from("root"), Bytes::new()]);
    assert_eq!(groups, [Bytes::from("staff")]);

    // One name per id, or the reply is rejected.
    let short = Reply::Extended(Bytes::from(s(&[&s(&[b"root"]), &s(&[b"staff"])])));
    let err = run(
        session.users_groups_by_id(&[0, 1000], &[1000]),
        &mut server,
        ext::USERS_GROUPS_BY_ID,
        &request,
        short,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Protocol(ProtocolError::UnexpectedReply(_))
    ));
}

#[tokio::test]
async fn nothing_is_sent_for_an_extension_the_server_did_not_announce() {
    let (session, mut server) = start(&[]).await;
    let h = Handle::from_bytes(Bytes::from_static(b"H"));
    assert!(!session.supports(ext::FSYNC));
    assert!(matches!(
        session.posix_rename("/a", "/b").await,
        Err(Error::Unsupported(ext::POSIX_RENAME))
    ));
    assert!(matches!(
        session.fsync(&h).await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        session.statvfs("/").await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(session.limits().await, Err(Error::Unsupported(_))));
    let io: std::io::Error = session.hardlink("/a", "/b").await.unwrap_err().into();
    assert_eq!(io.kind(), std::io::ErrorKind::Unsupported);
    assert!(
        timeout(Duration::from_millis(50), server.request())
            .await
            .is_err(),
        "a request reached the server"
    );
}

#[tokio::test]
async fn limits_are_asked_for_at_connect_time() {
    let (client, server) = duplex(64 * 1024);
    let (cr, cw) = split(client);
    let (sr, mut sw) = split(server);
    let connect = tokio::spawn(Session::connect(cr, cw, Config::default()));
    let mut srv = Server {
        reader: FrameReader::new(sr, 1 << 16),
        writer: split(duplex(8).0).1,
    };
    srv.reader.read_frame().await.unwrap().unwrap();
    let mut out = BytesMut::new();
    ServerPacket::Version(Hello {
        version: 3,
        extensions: vec![Extension {
            name: Bytes::from_static(b"limits@openssh.com"),
            data: Bytes::from_static(b"1"),
        }],
    })
    .encode_frame(&mut out)
    .unwrap();
    sw.write_all(&out).await.unwrap();
    // The client asks before `connect` returns.
    let r = srv.request().await;
    assert_eq!(
        r.op,
        Op::Extended {
            name: Bytes::from_static(b"limits@openssh.com"),
            data: Bytes::new()
        }
    );
    let mut out = BytesMut::new();
    ServerPacket::Response(Response {
        id: r.id,
        reply: Reply::Extended(u64s(&[262_144, 261_120, 261_120, 256])),
    })
    .encode_frame(&mut out)
    .unwrap();
    sw.write_all(&out).await.unwrap();
    let session = connect.await.unwrap().unwrap();
    let limits = session.known_limits().unwrap();
    assert_eq!(limits.max_packet_length, 262_144);
    assert_eq!(limits.max_open_handles, 256);
}
