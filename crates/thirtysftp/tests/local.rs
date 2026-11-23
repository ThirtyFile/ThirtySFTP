//! `LocalFs`: confinement to the root, and the ordinary operations on a real
//! disk, through the server and client.
#![cfg(all(feature = "client", feature = "server", any(unix, windows)))]

mod common;

use std::fs;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::Ordering;
#[cfg(unix)]
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::{Tmp, random_bytes};
use thirtysftp::client::{Config, File, OpenOptions, Session, fs as sftp_fs};
use thirtysftp::server::{Filesystem, FsError, LocalFs, ServerConfig, serve};
use thirtysftp::wire::{Attributes, OpenFlags, StatusCode, Times};
use tokio::io::{duplex, split};
#[cfg(unix)]
use tokio::time::timeout;

/// The scratch directory holds `root/`, which is served, and whatever a
/// test puts beside it.
trait Layout {
    fn root(&self) -> PathBuf;
    fn outside(&self) -> PathBuf;
}

impl Layout for Tmp {
    fn root(&self) -> PathBuf {
        let root = self.0.join("root");
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn outside(&self) -> PathBuf {
        self.0.clone()
    }
}

fn local(tmp: &Tmp) -> LocalFs {
    LocalFs::new(tmp.root()).unwrap()
}

async fn session(fs: LocalFs) -> Session {
    let (client, server) = duplex(1 << 20);
    let (sr, sw) = split(server);
    tokio::spawn(serve(sr, sw, Arc::new(fs), ServerConfig::default()));
    let (r, w) = split(client);
    Session::connect(r, w, Config::default()).await.unwrap()
}

const READ: OpenFlags = OpenFlags::READ;

async fn read_all(fs: &LocalFs, path: &str) -> Result<Vec<u8>, FsError> {
    let file = fs
        .open(path.as_bytes(), READ, &Attributes::default())
        .await?;
    let mut out = Vec::new();
    while let Some(data) = fs.read(&file, out.len() as u64, 65_536).await? {
        out.extend_from_slice(&data);
    }
    Ok(out)
}

fn code(e: &FsError) -> StatusCode {
    e.code
}

#[tokio::test]
async fn dot_dot_and_absolute_paths_stay_inside_the_root() {
    let tmp = Tmp::new();
    fs::write(tmp.outside().join("secret.txt"), "OUTSIDE").unwrap();
    let fs_ = local(&tmp);
    // Nothing named secret.txt inside yet: no way to reach the one outside.
    for path in [
        "/../secret.txt",
        "../secret.txt",
        "/a/../../secret.txt",
        "/../../../../../../secret.txt",
        "//..//secret.txt",
        "/./../secret.txt",
    ] {
        let err = read_all(&fs_, path).await.unwrap_err();
        assert_eq!(code(&err), StatusCode::NoSuchFile, "{path}");
    }
    // The same paths do reach a file of that name inside the root: `..`
    // stops at the root instead of leaving it.
    fs::write(tmp.root().join("secret.txt"), "INSIDE").unwrap();
    for path in ["/../secret.txt", "../../secret.txt", "/x/../../secret.txt"] {
        let got = if path.contains("/x/") {
            fs::create_dir_all(tmp.root().join("x")).unwrap();
            read_all(&fs_, path).await.unwrap()
        } else {
            read_all(&fs_, path).await.unwrap()
        };
        assert_eq!(got, b"INSIDE", "{path}");
    }
    // The host's own files are not there under an absolute name either.
    for path in ["/etc/passwd", "/Windows/win.ini", "/proc/self/environ"] {
        assert!(read_all(&fs_, path).await.is_err(), "{path}");
    }
    fs::create_dir_all(tmp.root().join("a")).unwrap();
    fs::create_dir_all(tmp.root().join("b/c")).unwrap();
    assert_eq!(
        &fs_.realpath(b"/a/../b/./c/../../..").await.unwrap()[..],
        b"/"
    );
    // The last name need not exist; the directories above it must.
    assert_eq!(&fs_.realpath(b"/b/new").await.unwrap()[..], b"/b/new");
    assert!(fs_.realpath(b"/nodir/new").await.is_err());
    assert_eq!(&fs_.realpath(b"/b/./c").await.unwrap()[..], b"/b/c");
    let root_attrs = fs_.stat(b"/..").await.unwrap();
    assert!(root_attrs.is_dir());
}

#[tokio::test]
async fn the_root_itself_cannot_be_removed_or_moved() {
    let tmp = Tmp::new();
    let fs_ = local(&tmp);
    assert!(fs_.rmdir(b"/").await.is_err());
    assert!(fs_.rmdir(b"/..").await.is_err());
    assert!(fs_.remove(b"/").await.is_err());
    fs_.mkdir(b"/d", &Attributes::default()).await.unwrap();
    assert!(fs_.rename(b"/", b"/d/root").await.is_err());
    assert!(fs_.rename(b"/d", b"/").await.is_err());
    assert!(tmp.root().is_dir());
    assert!(tmp.root().join("d").is_dir());
}

#[tokio::test]
async fn paths_with_nul_or_excessive_length_are_refused() {
    let tmp = Tmp::new();
    let fs_ = local(&tmp);
    for path in [b"/a\0b".to_vec(), vec![b'a'; 5000], b"\0".to_vec()] {
        let err = fs_.stat(&path).await.unwrap_err();
        assert_eq!(code(&err), StatusCode::Failure);
    }
}

#[tokio::test]
async fn read_only_mode_changes_nothing() {
    let tmp = Tmp::new();
    fs::write(tmp.root().join("f.txt"), "data").unwrap();
    fs::create_dir(tmp.root().join("d")).unwrap();
    let fs_ = local(&tmp).read_only(true);
    let denied = |r: Result<(), FsError>| {
        assert_eq!(code(&r.unwrap_err()), StatusCode::PermissionDenied);
    };
    let any = Attributes::default();
    for flags in [
        OpenFlags::WRITE,
        OpenFlags::READ | OpenFlags::WRITE,
        OpenFlags::APPEND,
        OpenFlags::READ | OpenFlags::CREATE,
        OpenFlags::READ | OpenFlags::TRUNCATE,
    ] {
        denied(fs_.open(b"/f.txt", flags, &any).await.map(|_| ()));
    }
    denied(fs_.remove(b"/f.txt").await);
    denied(fs_.mkdir(b"/n", &any).await);
    denied(fs_.rmdir(b"/d").await);
    denied(fs_.rename(b"/f.txt", b"/g.txt").await);
    denied(fs_.posix_rename(b"/f.txt", b"/g.txt").await);
    denied(fs_.symlink(b"f.txt", b"/l").await);
    denied(fs_.hardlink(b"/f.txt", b"/h").await);
    denied(
        fs_.setstat(
            b"/f.txt",
            &Attributes {
                size: Some(0),
                ..any.clone()
            },
        )
        .await,
    );
    denied(fs_.lsetstat(b"/f.txt", &any).await);
    assert_eq!(
        fs::read_to_string(tmp.root().join("f.txt")).unwrap(),
        "data"
    );
    assert!(!tmp.root().join("n").exists());

    // Reading, listing and statistics still work.
    assert_eq!(read_all(&fs_, "/f.txt").await.unwrap(), b"data");
    assert!(fs_.stat(b"/d").await.unwrap().is_dir());
    let dir = fs_.open_dir(b"/").await.unwrap();
    assert!(fs_.read_dir(&dir).await.unwrap().is_some());
    if let Ok(v) = fs_.statvfs(b"/").await {
        assert_ne!(v.flags & thirtysftp::extensions::StatVfs::READ_ONLY, 0);
    }
    // A file handle opened for reading cannot be written through.
    let file = fs_.open(b"/f.txt", READ, &any).await.unwrap();
    assert!(fs_.write(&file, 0, Bytes::from_static(b"x")).await.is_err());
    assert!(
        fs_.fsetstat(
            &file,
            &Attributes {
                size: Some(0),
                ..any
            }
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn files_and_directories_through_the_server() {
    let tmp = Tmp::new();
    let s = session(local(&tmp)).await;
    let data = random_bytes(700_000, 1);
    sftp_fs::write(&s, "/blob", data.clone()).await.unwrap();
    assert_eq!(fs::read(tmp.root().join("blob")).unwrap(), data);
    assert_eq!(sftp_fs::read(&s, "/blob").await.unwrap(), data);

    sftp_fs::create_dir_all(&s, "/a/b/c").await.unwrap();
    assert!(tmp.root().join("a/b/c").is_dir());
    sftp_fs::write(&s, "/a/b/c/f.txt", &b"hello"[..])
        .await
        .unwrap();
    let meta = sftp_fs::metadata(&s, "/a/b/c/f.txt").await.unwrap();
    assert!(meta.is_file());
    assert_eq!(meta.size, Some(5));
    assert!(sftp_fs::metadata(&s, "/a/b").await.unwrap().is_dir());

    // Rename refuses to replace, the replacing form does not.
    sftp_fs::write(&s, "/a/other", &b"other"[..]).await.unwrap();
    assert!(s.rename("/a/other", "/a/b/c/f.txt").await.is_err());
    assert_eq!(fs::read(tmp.root().join("a/b/c/f.txt")).unwrap(), b"hello");
    sftp_fs::rename(&s, "/a/other", "/a/b/c/f.txt")
        .await
        .unwrap();
    assert_eq!(fs::read(tmp.root().join("a/b/c/f.txt")).unwrap(), b"other");
    s.rename("/a/b", "/a/moved").await.unwrap();
    assert!(tmp.root().join("a/moved/c/f.txt").exists());

    // Errors keep their meaning.
    let err = sftp_fs::read(&s, "/missing").await.unwrap_err();
    assert_eq!(err.status_code(), Some(StatusCode::NoSuchFile));
    assert!(s.rmdir("/a").await.is_err(), "not empty");
    assert!(s.remove("/a").await.is_err(), "a directory");
    assert!(
        s.mkdir("/a", Attributes::default()).await.is_err(),
        "exists"
    );
    s.remove("/a/moved/c/f.txt").await.unwrap();
    s.rmdir("/a/moved/c").await.unwrap();
}

#[tokio::test]
async fn directory_listing_is_complete_and_carries_attributes() {
    let tmp = Tmp::new();
    for i in 0..600 {
        fs::write(tmp.root().join(format!("file{i:03}")), vec![b'x'; i]).unwrap();
    }
    fs::create_dir(tmp.root().join("sub")).unwrap();
    let s = session(local(&tmp)).await;
    let mut dir = sftp_fs::read_dir(&s, "/").await.unwrap();
    let mut seen = std::collections::BTreeMap::new();
    while let Some(entry) = dir.next_entry().await.unwrap() {
        let name = String::from_utf8(entry.name.to_vec()).unwrap();
        seen.insert(name, entry);
    }
    assert_eq!(seen.len(), 601);
    assert!(seen["sub"].attrs.is_dir());
    assert!(seen["file007"].attrs.is_file());
    assert_eq!(seen["file007"].attrs.size, Some(7));
    assert_eq!(seen["file599"].attrs.size, Some(599));
    assert!(!seen["file007"].long_name.is_empty());
    assert!(seen["file007"].attrs.times.is_some());
}

#[tokio::test]
async fn attributes_can_be_changed() {
    let tmp = Tmp::new();
    fs::write(tmp.root().join("f"), "0123456789").unwrap();
    let s = session(local(&tmp)).await;

    // Size, through the path and through a handle.
    s.setstat(
        "/f",
        Attributes {
            size: Some(4),
            ..Attributes::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(fs::read(tmp.root().join("f")).unwrap(), b"0123");
    let file = OpenOptions::new().write(true).open(&s, "/f").await.unwrap();
    file.set_len(8).await.unwrap();
    assert_eq!(fs::metadata(tmp.root().join("f")).unwrap().len(), 8);
    file.close().await.unwrap();

    // Times, to the second.
    let when = Times {
        accessed: 1_500_000_000,
        modified: 1_600_000_000,
    };
    s.setstat(
        "/f",
        Attributes {
            times: Some(when),
            ..Attributes::default()
        },
    )
    .await
    .unwrap();
    let got = s.stat("/f").await.unwrap().times.unwrap();
    assert_eq!(got.modified, when.modified);

    // Permissions: the whole mode on Unix, the read-only bit on Windows.
    s.setstat(
        "/f",
        Attributes {
            permissions: Some(0o444),
            ..Attributes::default()
        },
    )
    .await
    .unwrap();
    assert!(
        fs::metadata(tmp.root().join("f"))
            .unwrap()
            .permissions()
            .readonly()
    );
    let mode = s.stat("/f").await.unwrap().permissions.unwrap();
    assert_eq!(mode & 0o200, 0);
    s.setstat(
        "/f",
        Attributes {
            permissions: Some(0o644),
            ..Attributes::default()
        },
    )
    .await
    .unwrap();
    assert!(
        !fs::metadata(tmp.root().join("f"))
            .unwrap()
            .permissions()
            .readonly()
    );
    // Ownership cannot be set to the "unchanged" marker.
    #[cfg(unix)]
    {
        let err = s
            .setstat(
                "/f",
                Attributes {
                    owner: Some(thirtysftp::wire::Owner {
                        uid: u32::MAX,
                        gid: 0,
                    }),
                    ..Attributes::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.status_code(), Some(StatusCode::Failure));
    }
}

#[tokio::test]
async fn open_modes_behave_like_files() {
    let tmp = Tmp::new();
    let fs_ = local(&tmp);
    let any = Attributes::default();
    let rw = OpenFlags::READ | OpenFlags::WRITE;

    // CREATE makes it, EXCLUSIVE refuses an existing one.
    let f = fs_
        .open(b"/n", rw | OpenFlags::CREATE | OpenFlags::EXCLUSIVE, &any)
        .await
        .unwrap();
    fs_.write(&f, 0, Bytes::from_static(b"abcdef"))
        .await
        .unwrap();
    let err = fs_
        .open(b"/n", rw | OpenFlags::CREATE | OpenFlags::EXCLUSIVE, &any)
        .await
        .map(|_| ())
        .unwrap_err();
    assert_eq!(code(&err), StatusCode::Failure);
    // Without CREATE a missing file is missing.
    let err = fs_.open(b"/none", rw, &any).await.map(|_| ()).unwrap_err();
    assert_eq!(code(&err), StatusCode::NoSuchFile);
    // TRUNCATE empties it.
    drop(f);
    let f = fs_
        .open(b"/n", OpenFlags::WRITE | OpenFlags::TRUNCATE, &any)
        .await
        .unwrap();
    assert_eq!(fs::metadata(tmp.root().join("n")).unwrap().len(), 0);
    fs_.write(&f, 0, Bytes::from_static(b"12345"))
        .await
        .unwrap();
    drop(f);
    // APPEND writes at the end whatever the offset.
    let f = fs_.open(b"/n", OpenFlags::APPEND, &any).await.unwrap();
    fs_.write(&f, 0, Bytes::from_static(b"67")).await.unwrap();
    assert_eq!(fs::read(tmp.root().join("n")).unwrap(), b"1234567");
    // Reading a write-only handle, and writing a read-only one, is refused.
    assert!(fs_.read(&f, 0, 1).await.is_err());
    let r = fs_.open(b"/n", READ, &any).await.unwrap();
    assert!(fs_.write(&r, 0, Bytes::from_static(b"x")).await.is_err());
    // A directory is not a file.
    fs::create_dir(tmp.root().join("d")).unwrap();
    assert!(fs_.open(b"/d", READ, &any).await.is_err());
    assert!(fs_.open(b"/d", rw, &any).await.is_err());
    assert!(fs_.open(b"/", READ, &any).await.is_err());
    // Sparse writes far out, past 4 GiB.
    let f = fs_
        .open(b"/sparse", rw | OpenFlags::CREATE, &any)
        .await
        .unwrap();
    let at = (5u64 << 30) + 1;
    fs_.write(&f, at, Bytes::from_static(b"end")).await.unwrap();
    assert_eq!(&fs_.read(&f, at, 10).await.unwrap().unwrap()[..], b"end");
    assert_eq!(fs_.fstat(&f).await.unwrap().size, Some(at + 3));
    fs_.fsync(&f).await.unwrap();
    assert_eq!(fs_.read(&f, at + 3, 10).await.unwrap(), None);
    fs_.fsetstat(
        &f,
        &Attributes {
            size: Some(0),
            ..any
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn extensions_that_the_platform_offers_work() {
    let tmp = Tmp::new();
    fs::write(tmp.root().join("a"), "alpha").unwrap();
    fs::write(tmp.root().join("b"), "beta").unwrap();
    let s = session(local(&tmp)).await;

    s.posix_rename("/a", "/b").await.unwrap();
    assert_eq!(fs::read(tmp.root().join("b")).unwrap(), b"alpha");
    s.hardlink("/b", "/c").await.unwrap();
    assert_eq!(fs::read(tmp.root().join("c")).unwrap(), b"alpha");
    let f = File::open(&s, "/b").await.unwrap();
    f.sync_all().await.unwrap();
    fs::create_dir(tmp.root().join("x")).unwrap();
    assert_eq!(s.expand_path("~/x/../y").await.unwrap(), "/y");
    assert_eq!(s.home_directory("").await.unwrap(), "/");

    // copy-data, through the default read-and-write implementation.
    let data = random_bytes(300_000, 3);
    sftp_fs::write(&s, "/src", data.clone()).await.unwrap();
    let src = File::open(&s, "/src").await.unwrap();
    let dst = File::create(&s, "/dst").await.unwrap();
    s.copy_data(src.handle(), 100, 200_000, dst.handle(), 0)
        .await
        .unwrap();
    src.close().await.unwrap();
    dst.close().await.unwrap();
    assert_eq!(
        fs::read(tmp.root().join("dst")).unwrap(),
        data[100..200_100]
    );

    #[cfg(unix)]
    {
        let v = s.statvfs("/").await.unwrap();
        assert!(v.blocks > 0 && v.block_size > 0);
        assert!(s.fstatvfs(f.handle()).await.is_ok());
        // Account names are host information and are not offered unless
        // asked for.
        assert!(matches!(
            s.users_groups_by_id(&[0], &[0]).await,
            Err(thirtysftp::Error::Unsupported(_))
        ));
        let named = session(local(&tmp).account_names(true)).await;
        // Many ids in one request, most of them repeated or unknown.
        let mut uids = vec![0u32, 4_000_000_000];
        uids.extend((0..5000).map(|i| i % 7));
        let (users, groups) = named.users_groups_by_id(&uids, &[0]).await.unwrap();
        assert_eq!(users.len(), uids.len());
        assert_eq!(groups.len(), 1);
        assert!(users[1].is_empty(), "an id nobody has");
        assert_eq!(users[0], users[2], "the same id gives the same name");
    }
    #[cfg(windows)]
    {
        // Not offered, so the call is refused before it is sent.
        assert!(matches!(
            s.statvfs("/").await,
            Err(thirtysftp::Error::Unsupported(_))
        ));
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_names_with_special_meaning_are_refused() {
    let tmp = Tmp::new();
    fs::write(tmp.root().join("ok.txt"), "x").unwrap();
    let fs_ = local(&tmp);
    for path in [
        "/con",
        "/CON",
        "/nul.txt",
        "/aux",
        "/com1",
        "/LPT9.log",
        "/a:b",
        "/ok.txt:stream",
        "/C:",
        "/a\\b",
        "/..\\x",
        "/trailing.",
        "/space ",
        "/wild*",
        "/q?",
        "/<x>",
        "/p|q",
        "/\"q\"",
        "/ctl\u{1}",
    ] {
        let err = fs_
            .open(
                path.as_bytes(),
                OpenFlags::WRITE | OpenFlags::CREATE,
                &Attributes::default(),
            )
            .await
            .map(|_| ())
            .unwrap_err();
        assert_eq!(code(&err), StatusCode::Failure, "{path}");
        assert!(fs_.stat(path.as_bytes()).await.is_err(), "{path}");
    }
    // None of those left anything behind.
    let names: Vec<_> = fs::read_dir(tmp.root())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["ok.txt"]);
    // Ordinary names that look similar are fine.
    fs_.open(
        b"/console",
        OpenFlags::WRITE | OpenFlags::CREATE,
        &Attributes::default(),
    )
    .await
    .unwrap();
    fs_.open(
        b"/com10",
        OpenFlags::WRITE | OpenFlags::CREATE,
        &Attributes::default(),
    )
    .await
    .unwrap();
    assert!(fs_.symlink(b"x", b"/l").await.is_err());
    assert!(fs_.read_link(b"/ok.txt").await.is_err());
}

#[cfg(windows)]
#[tokio::test]
async fn windows_reparse_points_are_not_followed() {
    let tmp = Tmp::new();
    fs::write(tmp.outside().join("secret.txt"), "OUTSIDE").unwrap();
    let outside = tmp.outside();
    let link = tmp.root().join("link");
    // A symbolic link needs a privilege; a junction does not, and is a
    // reparse point just the same.
    let made_link = std::os::windows::fs::symlink_dir(&outside, &link).is_ok()
        || std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());
    if !made_link {
        eprintln!("cannot create a link or junction here; skipped");
        return;
    }
    let fs_ = local(&tmp);
    assert!(read_all(&fs_, "/link/secret.txt").await.is_err());
    assert!(
        fs_.open(
            b"/link/new.txt",
            OpenFlags::WRITE | OpenFlags::CREATE,
            &Attributes::default()
        )
        .await
        .is_err()
    );
    assert!(!outside.join("new.txt").exists());
    assert!(fs_.stat(b"/link").await.is_err());
    assert!(fs_.lstat(b"/link/secret.txt").await.is_err());
    assert!(fs_.open_dir(b"/link").await.map(|_| ()).is_err());
}

#[tokio::test]
async fn an_open_listing_does_not_pin_its_directory() {
    let tmp = Tmp::new();
    fs::create_dir(tmp.root().join("d")).unwrap();
    fs::write(tmp.root().join("d/f"), "x").unwrap();
    let fs_ = local(&tmp);
    // A client that opens a listing and walks away must not stop others
    // from renaming or removing the directory.
    let listing = fs_.open_dir(b"/d").await.unwrap();
    fs_.rename(b"/d", b"/e").await.unwrap();
    assert!(tmp.root().join("e/f").exists());
    fs_.remove(b"/e/f").await.unwrap();
    fs_.rmdir(b"/e").await.unwrap();
    assert!(!tmp.root().join("e").exists());
    // Reading the stale listing is harmless, whatever it returns.
    let _ = fs_.read_dir(&listing).await;
}

#[tokio::test]
async fn the_limit_on_concurrent_operations_does_not_get_in_the_way() {
    let tmp = Tmp::new();
    // One file system call at a time, with many requests in flight.
    let s = session(local(&tmp).max_operations(1)).await;
    let data = random_bytes(500_000, 11);
    sftp_fs::write(&s, "/one-at-a-time", data.clone())
        .await
        .unwrap();
    assert_eq!(sftp_fs::read(&s, "/one-at-a-time").await.unwrap(), data);
    // Zero is taken as one, not as "nothing may run".
    let s = session(local(&tmp).max_operations(0)).await;
    assert!(sftp_fs::metadata(&s, "/one-at-a-time").await.is_ok());
}

/// Demonstrates the known gap of `LocalFs` on Windows (see its module
/// documentation and `SECURITY.md`): a local process that turns a directory
/// of the served tree into a junction at the right moment can make a request
/// follow it. Reads are caught by the check after the operation in practice;
/// a file can still be created outside. This test fails when the race is
/// won, so it is not part of the normal run.
#[cfg(windows)]
#[tokio::test]
#[ignore = "known gap on Windows: a local junction swap can redirect a request"]
async fn windows_junction_swap_by_a_local_process() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    let tmp = Tmp::new();
    let outside = tmp.outside().join("outside_dir");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret.txt"), "OUTSIDE").unwrap();
    let fs_ = Arc::new(local(&tmp));
    let swap = tmp.root().join("swap");
    let junction = |link: &std::path::Path, target: &std::path::Path| {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if !junction(&swap, &outside) {
        eprintln!("cannot create a junction here; skipped");
        return;
    }
    let _ = fs::remove_dir(&swap);

    // The attacker turns /swap into a junction to the outside directory, a
    // real directory holding an inside file, and nothing, over and over.
    let stop = Arc::new(AtomicBool::new(false));
    let attacker = {
        let (swap, outside, stop) = (swap.clone(), outside.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut swaps = 0u32;
            while !stop.load(Ordering::Relaxed) {
                if junction(&swap, &outside) {
                    swaps += 1;
                    let _ = fs::remove_dir(&swap);
                }
                if fs::create_dir(&swap).is_ok() {
                    let _ = fs::write(swap.join("secret.txt"), "INSIDE");
                    std::thread::sleep(Duration::from_millis(2));
                    let _ = fs::remove_file(swap.join("secret.txt"));
                    let _ = fs::remove_dir(&swap);
                }
            }
            swaps
        })
    };
    let started = Instant::now();
    let (mut tries, mut hits) = (0u32, 0u32);
    while started.elapsed() < Duration::from_millis(2500) {
        tries += 1;
        if let Ok(data) = read_all(&fs_, "/swap/secret.txt").await {
            assert_eq!(data, b"INSIDE", "read through a swapped component");
            hits += 1;
        }
        let _ = fs_
            .open(
                b"/swap/planted.txt",
                OpenFlags::WRITE | OpenFlags::CREATE,
                &Attributes::default(),
            )
            .await;
        let _ = fs_.remove(b"/swap/planted.txt").await;
        let _ = fs_.stat(b"/swap/secret.txt").await;
    }
    stop.store(true, Ordering::Relaxed);
    let swaps = attacker.join().unwrap();
    eprintln!("{tries} attempts, {hits} reads of the inside file, {swaps} junction swaps");
    // The pinned directories slow the attacker down, which is the point; it
    // must still have got at least one junction in for the run to mean
    // anything.
    assert!(swaps >= 1, "the attacker never ran");
    assert!(!outside.join("planted.txt").exists());
    assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"OUTSIDE");
}

#[cfg(unix)]
mod unix_only {
    use super::*;
    use std::os::unix::fs::{MetadataExt, symlink};

    #[tokio::test]
    async fn symbolic_links_cannot_lead_out_of_the_root() {
        let tmp = Tmp::new();
        fs::write(tmp.outside().join("secret.txt"), "OUTSIDE").unwrap();
        fs::create_dir(tmp.outside().join("dir")).unwrap();
        fs::write(tmp.outside().join("dir/secret.txt"), "OUTSIDE").unwrap();
        fs::write(tmp.root().join("secret.txt"), "INSIDE").unwrap();
        let root = tmp.root();
        // Absolute links to real outside paths.
        symlink(tmp.outside().join("secret.txt"), root.join("abs_file")).unwrap();
        symlink(tmp.outside().join("dir"), root.join("abs_dir")).unwrap();
        symlink("/etc/passwd", root.join("etc_passwd")).unwrap();
        symlink("/", root.join("slash")).unwrap();
        // Relative links that climb above the root.
        symlink("../secret.txt", root.join("up_file")).unwrap();
        symlink("../../../..", root.join("up_far")).unwrap();
        symlink("../dir", root.join("up_dir")).unwrap();
        let fs_ = local(&tmp);

        // An absolute target starts at the root, so the outside file is not
        // there; nothing returns its content.
        for path in [
            "/abs_file",
            "/abs_dir/secret.txt",
            "/etc_passwd",
            "/slash/etc/passwd",
        ] {
            let err = read_all(&fs_, path).await.unwrap_err();
            assert_eq!(code(&err), StatusCode::NoSuchFile, "{path}");
        }
        // A relative target that climbs stops at the root, which holds a
        // file of the same name.
        assert_eq!(read_all(&fs_, "/up_file").await.unwrap(), b"INSIDE");
        assert_eq!(
            read_all(&fs_, "/up_far/secret.txt").await.unwrap(),
            b"INSIDE"
        );
        assert!(read_all(&fs_, "/up_dir/secret.txt").await.is_err());
        assert_eq!(
            &fs_.realpath(b"/up_far/up_far/secret.txt").await.unwrap()[..],
            b"/secret.txt"
        );

        // Creating through a link that leaves the root makes nothing outside.
        for path in ["/abs_dir/new.txt", "/up_dir/new.txt", "/abs_file2"] {
            let _ = fs_
                .open(
                    path.as_bytes(),
                    OpenFlags::WRITE | OpenFlags::CREATE,
                    &Attributes::default(),
                )
                .await;
        }
        assert!(!tmp.outside().join("dir/new.txt").exists());
        assert!(!tmp.outside().join("new.txt").exists());
        assert!(!tmp.outside().join("abs_file2").exists());
        // Changing attributes through a link does not touch the target.
        let before = fs::metadata(tmp.outside().join("secret.txt"))
            .unwrap()
            .mode();
        let _ = fs_
            .setstat(
                b"/abs_file",
                &Attributes {
                    permissions: Some(0o777),
                    size: Some(0),
                    ..Attributes::default()
                },
            )
            .await;
        assert_eq!(
            fs::metadata(tmp.outside().join("secret.txt"))
                .unwrap()
                .mode(),
            before
        );
        assert_eq!(
            fs::read(tmp.outside().join("secret.txt")).unwrap(),
            b"OUTSIDE"
        );
        // Removing a link removes the link.
        fs_.remove(b"/abs_file").await.unwrap();
        assert!(tmp.outside().join("secret.txt").exists());
        // Listing a directory reached through a link stays inside.
        let dir = fs_.open_dir(b"/up_far").await.unwrap();
        let mut names = Vec::new();
        while let Some(batch) = fs_.read_dir(&dir).await.unwrap() {
            names.extend(batch.into_iter().map(|e| e.name));
        }
        assert!(names.contains(&Bytes::from_static(b"secret.txt")));
        assert!(
            !names.contains(&Bytes::from_static(b"root")),
            "saw the parent"
        );
    }

    #[tokio::test]
    async fn links_inside_the_root_work_and_loops_are_caught() {
        let tmp = Tmp::new();
        let root = tmp.root();
        fs::create_dir(root.join("real")).unwrap();
        fs::write(root.join("real/file"), "payload").unwrap();
        symlink("real", root.join("rel")).unwrap();
        symlink("/real", root.join("abs")).unwrap();
        symlink("real/file", root.join("to_file")).unwrap();
        symlink("loop_b", root.join("loop_a")).unwrap();
        symlink("loop_a", root.join("loop_b")).unwrap();
        symlink("self", root.join("self")).unwrap();
        symlink("missing", root.join("dangling")).unwrap();
        let fs_ = local(&tmp);

        for path in ["/rel/file", "/abs/file", "/to_file", "/rel/../real/file"] {
            assert_eq!(read_all(&fs_, path).await.unwrap(), b"payload", "{path}");
        }
        assert_eq!(
            &fs_.realpath(b"/rel/file").await.unwrap()[..],
            b"/real/file"
        );
        assert_eq!(&fs_.realpath(b"/abs").await.unwrap()[..], b"/real");
        for path in ["/loop_a", "/self", "/loop_a/x"] {
            let err = read_all(&fs_, path).await.unwrap_err();
            assert_eq!(code(&err), StatusCode::Failure, "{path}");
        }
        assert_eq!(
            code(&read_all(&fs_, "/dangling").await.unwrap_err()),
            StatusCode::NoSuchFile
        );
        // stat follows, lstat does not; readlink returns the text.
        assert!(fs_.stat(b"/rel").await.unwrap().is_dir());
        assert!(fs_.lstat(b"/rel").await.unwrap().is_symlink());
        assert!(fs_.stat(b"/dangling").await.is_err());
        assert!(fs_.lstat(b"/dangling").await.unwrap().is_symlink());
        assert_eq!(&fs_.read_link(b"/abs").await.unwrap()[..], b"/real");
        assert!(fs_.read_link(b"/real").await.is_err(), "not a link");
        // Creating one through the server.
        fs_.symlink(b"real/file", b"/made").await.unwrap();
        assert_eq!(read_all(&fs_, "/made").await.unwrap(), b"payload");
        assert_eq!(
            fs::read_link(root.join("made")).unwrap(),
            Path::new("real/file")
        );
        // Writing through a dangling link creates its target inside.
        let f = fs_
            .open(
                b"/dangling",
                OpenFlags::WRITE | OpenFlags::CREATE,
                &Attributes::default(),
            )
            .await
            .unwrap();
        fs_.write(&f, 0, Bytes::from_static(b"made")).await.unwrap();
        assert_eq!(fs::read(root.join("missing")).unwrap(), b"made");
        // Removing a link to a directory removes the link, not the directory.
        fs_.remove(b"/rel").await.unwrap();
        assert!(root.join("real/file").exists());
        // lsetstat reaches the link itself; permissions on a link are refused.
        let times = Times {
            accessed: 1_400_000_000,
            modified: 1_400_000_000,
        };
        fs_.lsetstat(
            b"/abs",
            &Attributes {
                times: Some(times),
                ..Attributes::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            fs_.lstat(b"/abs").await.unwrap().times.unwrap().modified,
            times.modified
        );
        assert_ne!(
            fs_.stat(b"/real").await.unwrap().times.unwrap().modified,
            times.modified
        );
        let err = fs_
            .lsetstat(
                b"/abs",
                &Attributes {
                    permissions: Some(0o700),
                    ..Attributes::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(code(&err), StatusCode::OpUnsupported);
    }

    #[tokio::test]
    async fn swapping_a_directory_for_a_link_while_requests_run_never_leaks() {
        let tmp = Tmp::new();
        let outside_dir = tmp.outside().join("outside_dir");
        fs::create_dir(&outside_dir).unwrap();
        fs::write(outside_dir.join("secret.txt"), "OUTSIDE").unwrap();
        let fs_ = Arc::new(local(&tmp));
        let root = tmp.root();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // The attacker turns /swap into a directory holding an inside file,
        // a link to the outside directory (by absolute path and by a path
        // that climbs), and nothing, as fast as it can.
        let attacker = {
            let (root, outside, stop) = (root.clone(), outside_dir.clone(), stop.clone());
            std::thread::spawn(move || {
                let swap = root.join("swap");
                while !stop.load(Ordering::Relaxed) {
                    let _ = symlink(&outside, &swap);
                    let _ = fs::remove_file(&swap);
                    let _ = symlink("../outside_dir", &swap);
                    let _ = fs::remove_file(&swap);
                    if fs::create_dir(&swap).is_ok() {
                        let _ = fs::write(swap.join("secret.txt"), "INSIDE");
                        let _ = fs::remove_file(swap.join("secret.txt"));
                        let _ = fs::remove_dir(&swap);
                    }
                }
            })
        };
        let started = Instant::now();
        let (mut hits, mut tries) = (0u32, 0u32);
        while started.elapsed() < Duration::from_millis(1500) {
            tries += 1;
            let path = "/swap/secret.txt";
            if let Ok(data) = read_all(&fs_, path).await {
                assert_eq!(data, b"INSIDE", "read through a swapped component");
                hits += 1;
            }
            let _ = fs_
                .open(
                    b"/swap/planted.txt",
                    OpenFlags::WRITE | OpenFlags::CREATE,
                    &Attributes::default(),
                )
                .await;
            let _ = fs_.stat(b"/swap/secret.txt").await;
            let _ = fs_
                .setstat(
                    b"/swap/secret.txt",
                    &Attributes {
                        permissions: Some(0o600),
                        ..Attributes::default()
                    },
                )
                .await;
        }
        stop.store(true, Ordering::Relaxed);
        attacker.join().unwrap();
        eprintln!("{tries} attempts, {hits} reads of the inside file");
        assert!(tries > 50);
        assert!(!outside_dir.join("planted.txt").exists());
        assert_eq!(
            fs::read(outside_dir.join("secret.txt")).unwrap(),
            b"OUTSIDE"
        );
    }

    #[tokio::test]
    async fn set_user_id_bits_are_dropped_unless_allowed() {
        let tmp = Tmp::new();
        fs::write(tmp.root().join("f"), "x").unwrap();
        let mode = |name: &str| fs::metadata(tmp.root().join(name)).unwrap().mode() & 0o7777;
        let with = |bits: u32| Attributes {
            permissions: Some(bits),
            ..Attributes::default()
        };

        let fs_ = local(&tmp);
        fs_.setstat(b"/f", &with(0o6755)).await.unwrap();
        assert_eq!(mode("f"), 0o755, "set-user-id and set-group-id dropped");
        fs_.lsetstat(b"/f", &with(0o4711)).await.unwrap();
        assert_eq!(mode("f"), 0o711);
        let file = fs_
            .open(
                b"/made",
                OpenFlags::WRITE | OpenFlags::CREATE,
                &with(0o4700),
            )
            .await
            .unwrap();
        assert_eq!(mode("made"), 0o700);
        fs_.fsetstat(&file, &with(0o2750)).await.unwrap();
        assert_eq!(mode("made"), 0o750);
        fs_.mkdir(b"/dir", &with(0o2770)).await.unwrap();
        assert_eq!(mode("dir") & 0o6000, 0);
        // The sticky bit is not one of them.
        fs_.setstat(b"/dir", &with(0o1770)).await.unwrap();
        assert_eq!(mode("dir"), 0o1770);

        // Allowed when the server says so.
        let fs_ = local(&tmp).special_mode_bits(true);
        fs_.setstat(b"/f", &with(0o4755)).await.unwrap();
        assert_eq!(mode("f"), 0o4755);
    }

    #[tokio::test]
    async fn unix_attributes_and_permissions_are_the_files_own() {
        let tmp = Tmp::new();
        let fs_ = local(&tmp);
        let f = fs_
            .open(
                b"/p",
                OpenFlags::WRITE | OpenFlags::CREATE,
                &Attributes {
                    permissions: Some(0o600),
                    ..Attributes::default()
                },
            )
            .await
            .unwrap();
        drop(f);
        let md = fs::metadata(tmp.root().join("p")).unwrap();
        assert_eq!(md.mode() & 0o777, 0o600);
        let attrs = fs_.stat(b"/p").await.unwrap();
        assert_eq!(attrs.permissions.unwrap() & 0o170_000, 0o100_000);
        assert_eq!(attrs.permissions.unwrap() & 0o7777, 0o600);
        assert_eq!(attrs.owner.unwrap().uid, md.uid());
        fs_.mkdir(
            b"/pd",
            &Attributes {
                permissions: Some(0o700),
                ..Attributes::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            fs::metadata(tmp.root().join("pd")).unwrap().mode() & 0o777,
            0o700
        );
        // Changing the mode of a file we own works, whatever its mode.
        fs_.setstat(
            b"/p",
            &Attributes {
                permissions: Some(0o640),
                ..Attributes::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            fs::metadata(tmp.root().join("p")).unwrap().mode() & 0o777,
            0o640
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn names_that_are_not_utf8_survive() {
        use std::os::unix::ffi::OsStrExt;
        let tmp = Tmp::new();
        let raw = std::ffi::OsStr::from_bytes(b"caf\xe9-\xff.txt");
        fs::write(tmp.root().join(raw), "bytes").unwrap();
        let fs_ = local(&tmp);
        assert_eq!(
            read_all(&fs_, "/").await.map(|_| ()).unwrap_err().code,
            StatusCode::Failure
        );
        let dir = fs_.open_dir(b"/").await.unwrap();
        let entries = fs_.read_dir(&dir).await.unwrap().unwrap();
        let entry = entries
            .iter()
            .find(|e| e.name.starts_with(b"caf"))
            .expect("listed");
        assert_eq!(&entry.name[..], b"caf\xe9-\xff.txt");
        let file = fs_
            .open(b"/caf\xe9-\xff.txt", READ, &Attributes::default())
            .await
            .unwrap();
        assert_eq!(
            &fs_.read(&file, 0, 10).await.unwrap().unwrap()[..],
            b"bytes"
        );
    }

    #[tokio::test]
    async fn pipes_and_devices_are_refused_without_blocking() {
        let tmp = Tmp::new();
        let fifo = tmp.root().join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !matches!(made, Ok(s) if s.success()) {
            eprintln!("mkfifo is not available; skipped");
            return;
        }
        let fs_ = local(&tmp);
        for flags in [READ, OpenFlags::WRITE, OpenFlags::READ | OpenFlags::WRITE] {
            let result = timeout(
                Duration::from_secs(5),
                fs_.open(b"/pipe", flags, &Attributes::default()),
            )
            .await
            .expect("opening a pipe blocked");
            assert!(result.is_err());
        }
        // Truncating through the refused open changed nothing, and the entry
        // is still listed.
        assert!(fs_.lstat(b"/pipe").await.is_ok());
        let err = fs_
            .open(
                b"/pipe",
                OpenFlags::WRITE | OpenFlags::TRUNCATE,
                &Attributes::default(),
            )
            .await
            .map(|_| ())
            .unwrap_err();
        assert_eq!(code(&err), StatusCode::Failure);
    }

    #[tokio::test]
    async fn the_root_may_be_given_as_a_link_and_is_resolved_once() {
        let tmp = Tmp::new();
        fs::write(tmp.root().join("f"), "x").unwrap();
        let alias = tmp.outside().join("alias");
        symlink(tmp.root(), &alias).unwrap();
        let fs_ = LocalFs::new(&alias).unwrap();
        assert_eq!(read_all(&fs_, "/f").await.unwrap(), b"x");
        assert!(
            LocalFs::new(tmp.root().join("f")).is_err(),
            "not a directory"
        );
        assert!(LocalFs::new(tmp.root().join("nope")).is_err());
    }
}
