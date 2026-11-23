//! The high-level client against an in-memory server that stores files.
#![cfg(feature = "client")]

mod common;

use std::io::SeekFrom;
use std::time::Duration;

use common::{Opts, connect, random_bytes};
use thirtysftp::client::{Config, File, OpenOptions, fs};
use thirtysftp::wire::{Attributes, StatusCode};
use thirtysftp::{Error, ProtocolError};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::time::timeout;

#[tokio::test]
async fn upload_and_download_are_byte_identical_despite_reordering_and_short_reads() {
    let opts = Opts {
        max_read: 7000,
        delay: true,
        ..Opts::default()
    };
    let (session, server) = connect(opts, Config::default()).await;
    let data = random_bytes(1_500_000, 1);
    fs::write(&session, "/blob", data.clone()).await.unwrap();
    assert_eq!(server.lock().unwrap().contents("/blob"), data);
    assert_eq!(fs::read(&session, "/blob").await.unwrap(), data);
}

#[tokio::test]
async fn short_replies_are_made_up_before_later_data_is_used() {
    let opts = Opts {
        max_read: 1000,
        ..Opts::default()
    };
    let (session, server) = connect(opts, Config::default()).await;
    let data = random_bytes(200_000, 2);
    server.lock().unwrap().put("/f", &data);
    let file = File::open(&session, "/f").await.unwrap();
    assert_eq!(
        file.read_range(12_345, 100_000).await.unwrap(),
        data[12_345..112_345]
    );
    assert_eq!(file.read_all().await.unwrap(), data);
    file.close().await.unwrap();
}

#[tokio::test]
async fn reading_past_the_end_returns_what_exists() {
    let (session, server) = connect(Opts::default(), Config::default()).await;
    let data = random_bytes(100_000, 3);
    server.lock().unwrap().put("/f", &data);
    let file = File::open(&session, "/f").await.unwrap();
    assert_eq!(file.read_range(0, 1 << 20).await.unwrap(), data);
    assert_eq!(file.read_range(99_990, 100).await.unwrap(), data[99_990..]);
    assert!(file.read_range(100_000, 10).await.unwrap().is_empty());
    assert!(file.read_range(5 << 30, 10).await.unwrap().is_empty());
    assert!(file.read_range(0, 0).await.unwrap().is_empty());
    assert!(matches!(
        file.read_range(u64::MAX, 2).await,
        Err(Error::Io(_))
    ));
    let mut buf = [0u8; 16];
    assert_eq!(file.read_at(100_000, &mut buf).await.unwrap(), 0);
    assert_eq!(file.read_at(5, &mut buf).await.unwrap(), 16);
    assert_eq!(buf, data[5..21]);
}

#[tokio::test]
async fn offsets_beyond_four_gibibytes_work() {
    let (session, server) = connect(Opts::default(), Config::default()).await;
    let at = (5u64 << 30) + 3;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&session, "/big")
        .await
        .unwrap();
    file.write_all_at(at, &b"hello"[..]).await.unwrap();
    assert_eq!(file.read_range(at, 5).await.unwrap(), b"hello");
    assert_eq!(file.metadata().await.unwrap().size, Some(at + 5));
    assert_eq!(server.lock().unwrap().files[&b"/big"[..]].size(), at + 5);
    // The cursor moves there too.
    let mut file = file;
    assert_eq!(file.seek(SeekFrom::Start(at)).await.unwrap(), at);
    let mut five = [0u8; 5];
    file.read_exact(&mut five).await.unwrap();
    assert_eq!(&five, b"hello");
    assert_eq!(file.seek(SeekFrom::End(-5)).await.unwrap(), at);
    assert_eq!(file.seek(SeekFrom::Current(1)).await.unwrap(), at + 1);
    assert!(file.seek(SeekFrom::End(-(at as i64) - 100)).await.is_err());
    // A failed seek leaves the cursor where it was.
    assert_eq!(file.stream_position().await.unwrap(), at + 1);
}

#[tokio::test]
async fn a_failing_write_reports_its_error_and_leaves_the_session_usable() {
    let opts = Opts {
        fail_write: Some(5),
        ..Opts::default()
    };
    let (session, _server) = connect(opts, Config::default()).await;
    let file = File::create(&session, "/f").await.unwrap();
    let err = file
        .write_all_at(0, random_bytes(1 << 20, 4))
        .await
        .unwrap_err();
    assert_eq!(err.status_code(), Some(StatusCode::Failure));
    assert!(fs::try_exists(&session, "/f").await.unwrap());
}

#[tokio::test]
async fn async_io_adapters_read_write_and_seek() {
    let (session, server) = connect(Opts::default(), Config::default()).await;
    let data = random_bytes(300_000, 5);
    let mut file = File::create(&session, "/s").await.unwrap();
    for piece in data.chunks(10_007) {
        file.write_all(piece).await.unwrap();
    }
    file.shutdown().await.unwrap();
    assert_eq!(server.lock().unwrap().contents("/s"), data);
    assert_eq!(server.lock().unwrap().open_handles(), 0);

    let mut file = File::open(&session, "/s").await.unwrap();
    let mut all = Vec::new();
    file.read_to_end(&mut all).await.unwrap();
    assert_eq!(all, data);

    // Small reads are served from one larger reply.
    file.seek(SeekFrom::Start(1000)).await.unwrap();
    let mut seven = [0u8; 7];
    for i in 0..5 {
        file.read_exact(&mut seven).await.unwrap();
        assert_eq!(seven, data[1000 + i * 7..1007 + i * 7]);
    }
    assert_eq!(file.stream_position().await.unwrap(), 1035);
    // A seek throws the buffered data away.
    file.seek(SeekFrom::Start(0)).await.unwrap();
    file.read_exact(&mut seven).await.unwrap();
    assert_eq!(seven, data[..7]);
    file.seek(SeekFrom::End(-7)).await.unwrap();
    file.read_exact(&mut seven).await.unwrap();
    assert_eq!(seven, data[data.len() - 7..]);
    assert_eq!(file.read(&mut seven).await.unwrap(), 0);
}

#[tokio::test]
async fn a_write_error_surfaces_through_flush() {
    let opts = Opts {
        fail_write: Some(3),
        ..Opts::default()
    };
    let (session, _server) = connect(opts, Config::default()).await;
    let mut file = File::create(&session, "/f").await.unwrap();
    let mut failed = false;
    for _ in 0..8 {
        if file.write_all(&[7u8; 40_000]).await.is_err() {
            failed = true;
            break;
        }
    }
    let flushed = file.flush().await;
    assert!(failed || flushed.is_err(), "the failure was never reported");
    let err = file.shutdown().await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Other);
}

#[tokio::test]
async fn writes_and_reads_share_the_cursor_like_a_file() {
    let (session, server) = connect(Opts::default(), Config::default()).await;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&session, "/rw")
        .await
        .unwrap();
    file.write_all(b"0123456789").await.unwrap();
    file.flush().await.unwrap();
    file.seek(SeekFrom::Start(2)).await.unwrap();
    let mut two = [0u8; 2];
    file.read_exact(&mut two).await.unwrap();
    assert_eq!(&two, b"23");
    // Writing continues at the cursor, which the read moved to 4.
    file.write_all(b"XY").await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(server.lock().unwrap().contents("/rw"), b"0123XY6789");
}

#[tokio::test]
async fn directories_list_without_dot_entries() {
    let (session, server) = connect(Opts::default(), Config::default()).await;
    fs::create_dir_all(&session, "/a/b/c").await.unwrap();
    for name in ["f1", "f2", "f3", "f4", "f5", "f6", "f7"] {
        server.lock().unwrap().put(&format!("/a/{name}"), b"x");
    }
    let mut dir = fs::read_dir(&session, "/a").await.unwrap();
    let mut names = Vec::new();
    while let Some(entry) = dir.next_entry().await.unwrap() {
        names.push(String::from_utf8(entry.name.to_vec()).unwrap());
    }
    names.sort();
    assert_eq!(names, ["b", "f1", "f2", "f3", "f4", "f5", "f6", "f7"]);
    assert!(dir.next_entry().await.unwrap().is_none());
    dir.close().await.unwrap();
    assert_eq!(server.lock().unwrap().open_handles(), 0);
    assert!(fs::read_dir(&session, "/missing").await.is_err());
}

#[tokio::test]
async fn path_functions() {
    let (session, _server) = connect(Opts::default(), Config::default()).await;
    // Making a directory that exists is fine, a file in the way is not.
    fs::create_dir_all(&session, "/x/y").await.unwrap();
    fs::create_dir_all(&session, "/x/y/").await.unwrap();
    assert!(fs::metadata(&session, "/x/y").await.unwrap().is_dir());
    fs::write(&session, "/x/file", &b"data"[..]).await.unwrap();
    assert!(fs::create_dir_all(&session, "/x/file/z").await.is_err());
    assert!(fs::metadata(&session, "/x/file").await.unwrap().is_file());
    assert!(fs::try_exists(&session, "/x/file").await.unwrap());
    assert!(!fs::try_exists(&session, "/x/none").await.unwrap());

    fs::rename(&session, "/x/file", "/x/moved").await.unwrap();
    assert!(!fs::try_exists(&session, "/x/file").await.unwrap());
    assert_eq!(fs::read(&session, "/x/moved").await.unwrap(), b"data");
    fs::set_permissions(&session, "/x/moved", 0o600)
        .await
        .unwrap();
    fs::remove_file(&session, "/x/moved").await.unwrap();
    fs::remove_dir(&session, "/x/y").await.unwrap();
    let err = fs::read(&session, "/x/moved").await.unwrap_err();
    assert_eq!(err.status_code(), Some(StatusCode::NoSuchFile));
    assert_eq!(&fs::canonicalize(&session, ".").await.unwrap()[..], b"/");
}

#[tokio::test]
async fn rename_replaces_when_the_server_has_posix_rename() {
    let opts = Opts {
        posix_rename: true,
        ..Opts::default()
    };
    let (session, server) = connect(opts, Config::default()).await;
    server.lock().unwrap().put("/a", b"new");
    server.lock().unwrap().put("/b", b"old");
    // The plain request is refused by this server; the function picks the
    // extension.
    assert!(session.rename("/a", "/b").await.is_err());
    fs::rename(&session, "/a", "/b").await.unwrap();
    assert_eq!(server.lock().unwrap().contents("/b"), b"new");
}

#[tokio::test]
async fn open_options_check_their_combinations() {
    let (session, server) = connect(Opts::default(), Config::default()).await;
    for options in [
        OpenOptions::new(),
        OpenOptions::new().create(true).read(true).clone(),
        OpenOptions::new().read(true).truncate(true).clone(),
    ] {
        match options.open(&session, "/f").await {
            Err(Error::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(server.lock().unwrap().requests, 0);

    server.lock().unwrap().put("/exists", b"abc");
    let err = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&session, "/exists")
        .await
        .unwrap_err();
    assert_eq!(err.status_code(), Some(StatusCode::Failure));

    let file = OpenOptions::new()
        .append(true)
        .open(&session, "/exists")
        .await
        .unwrap();
    file.write_at(0, b"def").await.unwrap();
    file.close().await.unwrap();
    assert_eq!(server.lock().unwrap().contents("/exists"), b"abcdef");

    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&session, "/exists")
        .await
        .unwrap();
    assert_eq!(file.metadata().await.unwrap().size, Some(0));
    file.set_len(10).await.unwrap();
    assert_eq!(file.metadata().await.unwrap().size, Some(10));
}

#[tokio::test]
async fn transfers_respect_the_servers_limits() {
    let opts = Opts {
        limits: Some([8192, 4096, 2048, 100]),
        ..Opts::default()
    };
    let (session, server) = connect(opts, Config::default()).await;
    let limits = session.known_limits().copied().unwrap();
    assert_eq!(limits.max_read_length, 4096);
    assert_eq!(session.limits().await.unwrap(), limits);
    let data = random_bytes(100_000, 6);
    fs::write(&session, "/f", data.clone()).await.unwrap();
    assert_eq!(fs::read(&session, "/f").await.unwrap(), data);
    let fs_ = server.lock().unwrap();
    assert!(fs_.max_write_seen <= 2048, "{}", fs_.max_write_seen);
    assert!(fs_.max_read_seen <= 4096, "{}", fs_.max_read_seen);
}

#[tokio::test]
async fn without_limits_the_configured_chunk_size_is_used() {
    let (session, server) = connect(
        Opts::default(),
        Config::default()
            .with_chunk_size(1000)
            .with_pipeline_depth(3),
    )
    .await;
    assert!(session.known_limits().is_none());
    let data = random_bytes(50_000, 7);
    fs::write(&session, "/f", data.clone()).await.unwrap();
    assert_eq!(fs::read(&session, "/f").await.unwrap(), data);
    assert_eq!(server.lock().unwrap().max_write_seen, 1000);
}

#[tokio::test]
async fn a_server_that_returns_empty_reads_cannot_hang_the_client() {
    let opts = Opts {
        empty_reads: true,
        ..Opts::default()
    };
    let (session, server) = connect(opts, Config::default()).await;
    server.lock().unwrap().put("/f", b"content");
    let file = File::open(&session, "/f").await.unwrap();
    let ranged = timeout(Duration::from_secs(5), file.read_range(0, 100))
        .await
        .unwrap();
    assert!(matches!(
        ranged,
        Err(Error::Protocol(ProtocolError::NoProgress))
    ));
    let mut buf = [0u8; 8];
    assert!(matches!(
        file.read_at(0, &mut buf).await,
        Err(Error::Protocol(ProtocolError::NoProgress))
    ));
    let mut file = file;
    let read = timeout(Duration::from_secs(5), file.read(&mut buf))
        .await
        .unwrap();
    assert!(read.is_err());
}

#[tokio::test]
async fn dropping_a_file_or_directory_closes_its_handle() {
    let (session, server) = connect(Opts::default(), Config::default()).await;
    server.lock().unwrap().put("/f", b"x");
    let file = File::open(&session, "/f").await.unwrap();
    let dir = fs::read_dir(&session, "/").await.unwrap();
    assert_eq!(server.lock().unwrap().open_handles(), 2);
    drop(file);
    drop(dir);
    for _ in 0..100 {
        if server.lock().unwrap().open_handles() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("handles were not closed");
}

#[tokio::test]
async fn abandoning_a_transfer_leaves_the_session_sound() {
    let opts = Opts {
        delay: true,
        ..Opts::default()
    };
    let (session, server) = connect(opts, Config::default().with_pipeline_depth(8)).await;
    let data = random_bytes(2_000_000, 8);
    server.lock().unwrap().put("/f", &data);
    let file = File::open(&session, "/f").await.unwrap();
    for ms in [1, 3, 7, 15] {
        // Abandoned part way, or finished on a fast machine: both are fine,
        // what matters is the state the session is left in.
        let _ = timeout(Duration::from_millis(ms), file.read_range(0, 2_000_000)).await;
    }
    // Replies to the abandoned requests arrive meanwhile and are dropped.
    assert_eq!(file.read_range(0, 2_000_000).await.unwrap(), data);
    let upload = File::create(&session, "/g").await.unwrap();
    let cut = timeout(
        Duration::from_millis(3),
        upload.write_all_at(0, data.clone()),
    )
    .await;
    let _ = cut;
    upload.write_all_at(0, data.clone()).await.unwrap();
    upload.close().await.unwrap();
    assert_eq!(server.lock().unwrap().contents("/g"), data);
}

#[test]
fn file_type_comes_from_the_permission_bits() {
    let of = |mode| Attributes {
        permissions: Some(mode),
        ..Attributes::default()
    };
    assert!(of(0o100_644).is_file());
    assert!(of(0o040_755).is_dir());
    assert!(of(0o120_777).is_symlink());
    assert!(!of(0o020_600).is_file());
    assert!(!Attributes::default().is_dir());
    assert!(Attributes::default().file_type().is_none());
}

#[tokio::test]
async fn copying_a_remote_file_as_a_stream_is_byte_identical() {
    // Short replies and reordering, through `AsyncRead` this time.
    let opts = Opts {
        max_read: 5000,
        delay: true,
        ..Opts::default()
    };
    let (session, server) = connect(opts, Config::default()).await;
    let data = random_bytes(700_000, 9);
    server.lock().unwrap().put("/f", &data);
    let mut file = File::open(&session, "/f").await.unwrap();
    let mut copy = Vec::new();
    tokio::io::copy(&mut file, &mut copy).await.unwrap();
    assert_eq!(copy, data);
    // Reading again after the end finds nothing, and after a seek, the rest.
    let mut buf = [0u8; 10];
    assert_eq!(file.read(&mut buf).await.unwrap(), 0);
    file.seek(SeekFrom::Start(699_995)).await.unwrap();
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).await.unwrap();
    assert_eq!(tail, data[699_995..]);

    // And the other way: a stream written into a remote file.
    let mut upload = File::create(&session, "/g").await.unwrap();
    tokio::io::copy(&mut &data[..], &mut upload).await.unwrap();
    upload.shutdown().await.unwrap();
    assert_eq!(server.lock().unwrap().contents("/g"), data);
}
