//! Interoperability with OpenSSH.
//!
//! Every test here is `#[ignore]`d because it needs OpenSSH programs. Run
//! them with `cargo test -p thirtysftp --test openssh -- --ignored`. They
//! need `ssh-keygen`, `sftp` and `scp` on the `PATH`, plus the `sftp-server`
//! and `sshd` programs, which are looked for in the usual places and can be
//! named with the `SFTP_SERVER` and `SSHD` environment variables (and
//! `SSHD_LIBEXEC` for the directory of `sshd`'s helper programs, if it is not
//! installed in its usual place). `sshd`
//! runs as the current user on the loopback interface, which needs the
//! privilege separation directory (`/run/sshd` on Linux) to exist.
//!
//! Three directions are covered:
//! - this crate's client against OpenSSH's `sftp-server`, over a pipe;
//! - this crate's client, over ThirtySSH, against a running OpenSSH `sshd`;
//! - OpenSSH's `sftp` and `scp` against this crate's server.
#![cfg(all(unix, feature = "client", feature = "server", feature = "thirtyssh"))]

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::{Tmp, random_bytes};
use data_encoding::BASE64;
use thirtysftp::client::{Config, File, OpenOptions, Session, extensions as ext, fs as sftp_fs};
use thirtysftp::server::{LocalFs, ServerConfig};
use thirtysftp::wire::{Attributes, OpenFlags, Owner, StatusCode, Times};
use thirtysftp::{Error, ssh};
use thirtyssh::keys::{Algorithm, Credential, PrivateKeyReader};
use thirtyssh::{client, server, session, transport::HostKey};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

fn find(env: &str, candidates: &[&str]) -> PathBuf {
    if let Ok(path) = std::env::var(env) {
        return PathBuf::from(path);
    }
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .unwrap_or_else(|| panic!("not found; set {env}: tried {candidates:?}"))
}

fn sftp_server_path() -> PathBuf {
    find(
        "SFTP_SERVER",
        &[
            "/usr/lib/openssh/sftp-server",
            "/usr/libexec/openssh/sftp-server",
            "/usr/libexec/sftp-server",
            "/usr/lib/ssh/sftp-server",
        ],
    )
}

fn sshd_path() -> PathBuf {
    find(
        "SSHD",
        &["/usr/sbin/sshd", "/usr/local/sbin/sshd", "/usr/bin/sshd"],
    )
}

fn user_name() -> String {
    let out = std::process::Command::new("id")
        .arg("-un")
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| {
            o.stdout
                == b"0
"
        })
}

// ----------------------------------------------------------------------
// This crate's client against `sftp-server` on a pipe.

struct Pipe {
    session: Session,
    _child: Child,
}

async fn spawn_sftp_server(config: Config) -> Pipe {
    let mut child = Command::new(sftp_server_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("cannot start sftp-server");
    let (stdin, stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
    let session = timeout(
        Duration::from_secs(10),
        Session::connect(stdout, stdin, config),
    )
    .await
    .expect("handshake timed out")
    .expect("handshake failed");
    Pipe {
        session,
        _child: child,
    }
}

async fn pipe() -> Pipe {
    spawn_sftp_server(Config::default()).await
}

#[tokio::test]
#[ignore = "needs OpenSSH's sftp-server"]
async fn sftp_server_handshake_and_limits() {
    let p = pipe().await;
    let s = &p.session;
    for e in s.extensions() {
        eprintln!(
            "extension {} = {}",
            String::from_utf8_lossy(&e.name),
            String::from_utf8_lossy(&e.data)
        );
    }
    for name in [
        ext::POSIX_RENAME,
        ext::STATVFS,
        ext::FSTATVFS,
        ext::HARDLINK,
        ext::FSYNC,
    ] {
        assert!(s.supports(name), "sftp-server does not announce {name}");
    }
    assert_eq!(s.extension(ext::STATVFS).unwrap(), "2");
    if let Some(limits) = s.known_limits() {
        eprintln!("limits {limits:?}");
        assert!(limits.max_packet_length > 0);
    }
}

#[tokio::test]
#[ignore = "needs OpenSSH's sftp-server"]
async fn sftp_server_transfers_are_byte_identical() {
    let tmp = Tmp::new();
    // The default chunk size, and one larger than the server will answer in
    // one piece, which makes it return short reads that must be made up.
    for (chunk, depth) in [(32 * 1024, 64), (1024 * 1024, 8)] {
        let p = spawn_sftp_server(
            Config::default()
                .with_chunk_size(chunk)
                .with_pipeline_depth(depth),
        )
        .await;
        let s = &p.session;
        let data = random_bytes(6 * 1024 * 1024 + 123, u64::from(chunk));
        let path = tmp.remote(&format!("blob-{chunk}"));
        sftp_fs::write(s, &path, data.clone()).await.unwrap();
        assert_eq!(fs::read(&path).unwrap(), data, "upload, chunk {chunk}");
        assert_eq!(
            sftp_fs::read(s, &path).await.unwrap(),
            data,
            "download, chunk {chunk}"
        );
        // A ranged read in the middle, and one that runs past the end.
        let file = File::open(s, &path).await.unwrap();
        assert_eq!(
            file.read_range(1_000_001, 2_000_003).await.unwrap(),
            data[1_000_001..3_000_004]
        );
        assert_eq!(
            file.read_range(data.len() as u64 - 10, 100).await.unwrap(),
            data[data.len() - 10..]
        );
        file.close().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "needs OpenSSH's sftp-server"]
async fn sftp_server_files_directories_and_errors() {
    let tmp = Tmp::new();
    let p = pipe().await;
    let s = &p.session;
    let dir = tmp.remote("d");
    s.mkdir(&dir, Attributes::default()).await.unwrap();
    assert!(fs::metadata(tmp.path("d")).unwrap().is_dir());
    assert_eq!(
        s.mkdir(&dir, Attributes::default())
            .await
            .unwrap_err()
            .status_code(),
        Some(StatusCode::Failure)
    );
    sftp_fs::write(s, format!("{dir}/a"), &b"alpha"[..])
        .await
        .unwrap();
    sftp_fs::write(s, format!("{dir}/b"), &b"beta"[..])
        .await
        .unwrap();
    s.rename(format!("{dir}/a"), format!("{dir}/c"))
        .await
        .unwrap();
    assert!(fs::metadata(tmp.path("d/c")).is_ok());

    let mut names = Vec::new();
    let mut listing = sftp_fs::read_dir(s, &dir).await.unwrap();
    while let Some(entry) = listing.next_entry().await.unwrap() {
        names.push(String::from_utf8(entry.name.to_vec()).unwrap());
    }
    names.sort();
    assert_eq!(names, ["b", "c"]);

    let attrs = s.stat(format!("{dir}/c")).await.unwrap();
    assert!(attrs.is_file());
    assert_eq!(attrs.size, Some(5));
    assert_eq!(
        attrs.owner.unwrap().uid,
        fs::metadata(tmp.path("d/c")).unwrap().uid()
    );

    for (result, want) in [
        (
            s.stat(tmp.remote("missing")).await.map(|_| ()),
            StatusCode::NoSuchFile,
        ),
        (
            s.remove(tmp.remote("missing")).await,
            StatusCode::NoSuchFile,
        ),
        (s.rmdir(&dir).await, StatusCode::Failure),
    ] {
        assert_eq!(result.unwrap_err().status_code(), Some(want));
    }
    if !is_root() {
        fs::set_permissions(tmp.path("d"), fs::Permissions::from_mode(0o500)).unwrap();
        let err = sftp_fs::write(s, format!("{dir}/denied"), &b"x"[..])
            .await
            .unwrap_err();
        assert_eq!(err.status_code(), Some(StatusCode::PermissionDenied));
        fs::set_permissions(tmp.path("d"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    s.remove(format!("{dir}/b")).await.unwrap();
    s.remove(format!("{dir}/c")).await.unwrap();
    s.rmdir(&dir).await.unwrap();
}

#[tokio::test]
#[ignore = "needs OpenSSH's sftp-server"]
async fn sftp_server_symlink_arguments_are_in_openssh_order() {
    let tmp = Tmp::new();
    let p = pipe().await;
    let s = &p.session;
    let link = tmp.remote("link");
    s.symlink("target-text", &link).await.unwrap();
    // The link is `link`, and what it points to is `target-text`.
    assert_eq!(
        fs::read_link(tmp.path("link")).unwrap(),
        Path::new("target-text")
    );
    assert_eq!(s.readlink(&link).await.unwrap(), "target-text");
    assert!(s.lstat(&link).await.unwrap().is_symlink());
    assert_eq!(
        s.stat(&link).await.unwrap_err().status_code(),
        Some(StatusCode::NoSuchFile),
        "dangling link"
    );
    fs::write(tmp.path("target-text"), "t").unwrap();
    assert!(s.stat(&link).await.unwrap().is_file());
    assert_eq!(
        s.realpath(&link).await.unwrap(),
        fs::canonicalize(tmp.path("target-text"))
            .unwrap()
            .to_str()
            .unwrap()
            .as_bytes()
    );
}

#[tokio::test]
#[ignore = "needs OpenSSH's sftp-server"]
async fn sftp_server_attributes() {
    let tmp = Tmp::new();
    let p = pipe().await;
    let s = &p.session;
    let path = tmp.remote("f");
    fs::write(tmp.path("f"), "0123456789").unwrap();

    s.setstat(
        &path,
        Attributes {
            permissions: Some(0o640),
            ..Attributes::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(fs::metadata(tmp.path("f")).unwrap().mode() & 0o7777, 0o640);

    let when = Times {
        accessed: 1_500_000_000,
        modified: 1_600_000_000,
    };
    s.setstat(
        &path,
        Attributes {
            times: Some(when),
            ..Attributes::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(fs::metadata(tmp.path("f")).unwrap().mtime(), 1_600_000_000);
    assert_eq!(
        s.stat(&path).await.unwrap().times.unwrap().modified,
        1_600_000_000
    );

    s.setstat(
        &path,
        Attributes {
            size: Some(4),
            ..Attributes::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(fs::read(tmp.path("f")).unwrap(), b"0123");

    // Owner: our own ids, which any user may "change" to.
    let md = fs::metadata(tmp.path("f")).unwrap();
    s.setstat(
        &path,
        Attributes {
            owner: Some(Owner {
                uid: md.uid(),
                gid: md.gid(),
            }),
            ..Attributes::default()
        },
    )
    .await
    .unwrap();

    let file = OpenOptions::new().write(true).open(s, &path).await.unwrap();
    file.set_len(2).await.unwrap();
    assert_eq!(fs::read(tmp.path("f")).unwrap(), b"01");
    file.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs OpenSSH's sftp-server"]
async fn sftp_server_extensions() {
    let tmp = Tmp::new();
    let p = pipe().await;
    let s = &p.session;
    fs::write(tmp.path("a"), "alpha").unwrap();
    fs::write(tmp.path("b"), "beta").unwrap();
    let (a, b, c) = (tmp.remote("a"), tmp.remote("b"), tmp.remote("c"));

    // Plain rename does not replace; the extension does.
    assert!(s.rename(&a, &b).await.is_err());
    s.posix_rename(&a, &b).await.unwrap();
    assert_eq!(fs::read(tmp.path("b")).unwrap(), b"alpha");
    assert!(!tmp.path("a").exists());

    s.hardlink(&b, &c).await.unwrap();
    assert_eq!(fs::metadata(tmp.path("b")).unwrap().nlink(), 2);

    let v = s.statvfs(tmp.remote("")).await.unwrap();
    assert!(v.blocks > 0 && v.block_size > 0);
    let file = File::open(s, &b).await.unwrap();
    assert_eq!(
        s.fstatvfs(file.handle()).await.unwrap().block_size,
        v.block_size
    );
    if s.supports(ext::FSYNC) {
        let w = OpenOptions::new().write(true).open(s, &b).await.unwrap();
        w.sync_all().await.unwrap();
        w.close().await.unwrap();
    }
    if s.supports(ext::LSETSTAT) {
        s.symlink("b", tmp.remote("sl")).await.unwrap();
        s.lsetstat(
            tmp.remote("sl"),
            &Attributes {
                times: Some(Times {
                    accessed: 1_400_000_000,
                    modified: 1_400_000_000,
                }),
                ..Attributes::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            fs::symlink_metadata(tmp.path("sl")).unwrap().mtime(),
            1_400_000_000
        );
        assert_ne!(fs::metadata(tmp.path("b")).unwrap().mtime(), 1_400_000_000);
    }
    if s.supports(ext::EXPAND_PATH) {
        let home = s.expand_path("~").await.unwrap();
        assert!(home.starts_with(b"/"), "{home:?}");
        let here = s.expand_path(tmp.remote("")).await.unwrap();
        assert_eq!(
            fs::canonicalize(String::from_utf8(here.to_vec()).unwrap()).unwrap(),
            fs::canonicalize(tmp.path("")).unwrap()
        );
    } else {
        assert!(matches!(
            s.expand_path("~").await,
            Err(Error::Unsupported(_))
        ));
    }
    if s.supports(ext::USERS_GROUPS_BY_ID) {
        let md = fs::metadata(tmp.path("b")).unwrap();
        let (users, groups) = s
            .users_groups_by_id(&[md.uid()], &[md.gid()])
            .await
            .unwrap();
        assert_eq!(users[0], user_name().as_bytes());
        assert_eq!(groups.len(), 1);
    }
    if s.supports(ext::COPY_DATA) {
        let src = File::open(s, &b).await.unwrap();
        let dst = File::create(s, &tmp.remote("copy")).await.unwrap();
        s.copy_data(src.handle(), 1, 3, dst.handle(), 0)
            .await
            .unwrap();
        src.close().await.unwrap();
        dst.close().await.unwrap();
        assert_eq!(fs::read(tmp.path("copy")).unwrap(), b"lph");
    }
    if !s.supports(ext::HOME_DIRECTORY) {
        assert!(matches!(
            s.home_directory("").await,
            Err(Error::Unsupported(_))
        ));
    }

    // An extension nobody has is refused with a status, and the session goes on.
    let err = s
        .extended("nonsense@example.test", Bytes::new())
        .await
        .unwrap_err();
    assert_eq!(err.status_code(), Some(StatusCode::OpUnsupported));
    assert!(s.stat(&b).await.is_ok());
    file.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs OpenSSH's sftp-server"]
async fn sftp_server_open_modes_and_big_offsets() {
    let tmp = Tmp::new();
    let p = pipe().await;
    let s = &p.session;
    let path = tmp.remote("f");
    let flags = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE;
    let h = s.open(&path, flags, Attributes::default()).await.unwrap();
    s.write(&h, 0, Bytes::from_static(b"abcdef")).await.unwrap();
    s.close_handle(&h).await.unwrap();
    // Exclusive creation of an existing file fails.
    let err = s
        .open(&path, flags | OpenFlags::EXCLUSIVE, Attributes::default())
        .await
        .unwrap_err();
    assert!(err.status_code().is_some());
    // Truncate empties it, append adds at the end.
    let h = s
        .open(
            &path,
            OpenFlags::WRITE | OpenFlags::TRUNCATE,
            Attributes::default(),
        )
        .await
        .unwrap();
    s.write(&h, 0, Bytes::from_static(b"12")).await.unwrap();
    s.close_handle(&h).await.unwrap();
    let h = s
        .open(
            &path,
            OpenFlags::WRITE | OpenFlags::APPEND,
            Attributes::default(),
        )
        .await
        .unwrap();
    s.write(&h, 0, Bytes::from_static(b"34")).await.unwrap();
    s.close_handle(&h).await.unwrap();
    assert_eq!(fs::read(tmp.path("f")).unwrap(), b"1234");

    // An offset past 4 GiB: the file is sparse on any real file system.
    let at = (5u64 << 30) + 7;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(s, &path)
        .await
        .unwrap();
    file.write_all_at(at, &b"far"[..]).await.unwrap();
    assert_eq!(fs::metadata(tmp.path("f")).unwrap().len(), at + 3);
    assert_eq!(file.read_range(at, 3).await.unwrap(), b"far");
    assert_eq!(file.metadata().await.unwrap().size, Some(at + 3));
    file.set_len(4).await.unwrap();
    file.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs OpenSSH's sftp-server"]
async fn sftp_server_lists_a_large_directory() {
    let tmp = Tmp::new();
    fs::create_dir(tmp.path("big")).unwrap();
    for i in 0..3000 {
        fs::write(tmp.path(&format!("big/file{i:05}")), "").unwrap();
    }
    let p = pipe().await;
    let mut dir = sftp_fs::read_dir(&p.session, tmp.remote("big"))
        .await
        .unwrap();
    let mut count = 0;
    while dir.next_entry().await.unwrap().is_some() {
        count += 1;
    }
    assert_eq!(count, 3000);
}

#[tokio::test]
#[ignore = "needs OpenSSH's sftp-server"]
async fn sftp_server_many_requests_at_once() {
    let tmp = Tmp::new();
    let p = pipe().await;
    let s = p.session.clone();
    let mut tasks = Vec::new();
    for i in 0..16u64 {
        let (s, path) = (s.clone(), tmp.remote(&format!("p{i}")));
        tasks.push(tokio::spawn(async move {
            let data = random_bytes(300_000, i);
            sftp_fs::write(&s, &path, data.clone()).await.unwrap();
            assert_eq!(sftp_fs::read(&s, &path).await.unwrap(), data);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
}

// ----------------------------------------------------------------------
// Keys for the tests that go through SSH.

struct Keys {
    dir: Tmp,
}

impl Keys {
    fn generate() -> Self {
        let dir = Tmp::new();
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(dir.path("client"))
            .status()
            .expect("ssh-keygen is needed");
        assert!(status.success());
        Self { dir }
    }

    fn private(&self) -> PathBuf {
        self.dir.path("client")
    }

    fn public(&self) -> PathBuf {
        self.dir.path("client.pub")
    }
}

/// The 32 key bytes of an `ssh-ed25519` public key file.
fn ed25519_public(path: &Path) -> [u8; 32] {
    const PREFIX: &[u8] = b"\0\0\0\x0bssh-ed25519\0\0\0\x20";
    let text = fs::read_to_string(path).unwrap();
    let blob = BASE64
        .decode(text.split_whitespace().nth(1).unwrap().as_bytes())
        .unwrap();
    blob.strip_prefix(PREFIX).unwrap().try_into().unwrap()
}

// ----------------------------------------------------------------------
// This crate's client, over ThirtySSH, against a running `sshd`.

struct Sshd {
    child: Child,
    port: u16,
    host_key: [u8; 32],
    _dir: Tmp,
}

async fn start_sshd(client_public: &Path) -> Sshd {
    let dir = Tmp::new();
    let status = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(dir.path("host"))
        .status()
        .unwrap();
    assert!(status.success());
    fs::copy(client_public, dir.path("authorized_keys")).unwrap();
    fs::set_permissions(
        dir.path("authorized_keys"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    // The port is chosen by binding and releasing it, so another program can
    // take it in between. Whatever answers must be an SSH server, and if
    // `sshd` does not come up on the port, another one is tried.
    for _ in 0..5 {
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        // `sshd` 9.8 and later start helper programs from a compiled-in path. A
        // copy unpacked elsewhere is pointed at its helpers with `SSHD_LIBEXEC`.
        let helpers = std::env::var("SSHD_LIBEXEC").map_or_else(
            |_| String::new(),
            |dir| format!("SshdSessionPath {dir}/sshd-session\nSshdAuthPath {dir}/sshd-auth\n"),
        );
        let config = format!(
            "{helpers}Port {port}\nListenAddress 127.0.0.1\nHostKey {host}\nPidFile {pid}\n\
             AuthorizedKeysFile {keys}\nPubkeyAuthentication yes\n\
             PasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\n\
             StrictModes no\nLogLevel ERROR\nSubsystem sftp {server}\n",
            host = dir.path("host").display(),
            pid = dir.path("sshd.pid").display(),
            keys = dir.path("authorized_keys").display(),
            server = sftp_server_path().display(),
        );
        fs::write(dir.path("sshd_config"), config).unwrap();
        let mut child = Command::new(sshd_path())
            .args(["-D", "-e", "-f"])
            .arg(dir.path("sshd_config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("cannot start sshd");
        for _ in 0..100 {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if is_ssh_server(port).await {
                let host_key = ed25519_public(&dir.path("host.pub"));
                return Sshd {
                    child,
                    port,
                    host_key,
                    _dir: dir,
                };
            }
            sleep(Duration::from_millis(50)).await;
        }
    }
    panic!("sshd did not start listening");
}

/// Whether an SSH server answers on `port`.
async fn is_ssh_server(port: u16) -> bool {
    use tokio::io::AsyncReadExt;
    let Ok(mut stream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
        return false;
    };
    let mut banner = [0u8; 8];
    matches!(
        timeout(Duration::from_secs(2), stream.read_exact(&mut banner)).await,
        Ok(Ok(_))
    ) && banner.starts_with(b"SSH-2.0-")
}

struct Trusting {
    pin: [u8; 32],
}

impl client::Handler for Trusting {
    async fn verify_host(&mut self, host: &Credential, _: &Algorithm) -> std::io::Result<bool> {
        Ok(host.ed25519().is_some_and(|key| key == self.pin))
    }
}

impl session::Handler for Trusting {}

#[tokio::test]
#[ignore = "needs OpenSSH's sshd and sftp-server"]
async fn client_over_ssh_against_sshd() {
    let keys = Keys::generate();
    let sshd = start_sshd(&keys.public()).await;
    let mut connection = client::connect(
        Arc::new(client::Config::default()),
        ("127.0.0.1", sshd.port),
        Trusting { pin: sshd.host_key },
    )
    .await
    .unwrap();
    let identity = PrivateKeyReader::new().file(keys.private()).unwrap();
    assert!(
        connection
            .authenticate(
                &user_name(),
                client::Method::Key {
                    key: &identity,
                    algorithm: None,
                },
            )
            .await
            .unwrap()
            .is_accepted(),
        "sshd refused the key"
    );
    let ssh_session = connection.start().unwrap();
    let handle = ssh_session.handle();

    let tmp = Tmp::new();
    let sftp = timeout(
        Duration::from_secs(20),
        ssh::connect(&handle, Config::default()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(sftp.supports(ext::POSIX_RENAME));
    let data = random_bytes(5 * 1024 * 1024, 9);
    let path = tmp.remote("through-sshd");
    sftp_fs::write(&sftp, &path, data.clone()).await.unwrap();
    assert_eq!(fs::read(&path).unwrap(), data);
    assert_eq!(sftp_fs::read(&sftp, &path).await.unwrap(), data);
    sftp_fs::create_dir_all(&sftp, tmp.remote("a/b/c"))
        .await
        .unwrap();
    assert!(tmp.path("a/b/c").is_dir());

    // Two sessions at once, and a rekey while one transfers.
    let second = ssh::connect(&handle, Config::default()).await.unwrap();
    let rekey = tokio::spawn({
        let handle = handle.clone();
        async move {
            for _ in 0..3 {
                handle.rekey().await.unwrap();
                sleep(Duration::from_millis(30)).await;
            }
        }
    });
    let again = random_bytes(4 * 1024 * 1024, 10);
    sftp_fs::write(&second, tmp.remote("during-rekey"), again.clone())
        .await
        .unwrap();
    rekey.await.unwrap();
    assert_eq!(fs::read(tmp.path("during-rekey")).unwrap(), again);
    drop((sftp, second));
    ssh_session.disconnect().await.unwrap();
    drop(sshd.child);
}

// ----------------------------------------------------------------------
// OpenSSH's `sftp` and `scp` against this crate's server.

struct OurServer {
    port: u16,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    root: Tmp,
    keys: Keys,
}

#[derive(Clone)]
struct Conn {
    fs: Arc<LocalFs>,
    key: [u8; 32],
    user: String,
}

impl server::AuthPolicy for Conn {
    async fn public_key(&mut self, user: &str, key: &[u8; 32]) -> std::io::Result<bool> {
        Ok(user == self.user && *key == self.key)
    }
}

impl session::Handler for Conn {
    async fn channel_open(
        &mut self,
        request: session::IncomingChannel,
        _: &session::Context,
    ) -> std::io::Result<()> {
        ssh::accept_session(request, self.fs.clone(), ServerConfig::default()).await
    }
}

struct Factory(Conn);

impl server::Connections for Factory {
    type Handler = Conn;

    fn handler(&mut self, _: std::net::SocketAddr) -> Conn {
        self.0.clone()
    }
}

async fn our_server() -> OurServer {
    let keys = Keys::generate();
    let root = Tmp::new();
    fs::create_dir(root.path("root")).unwrap();
    let conn = Conn {
        fs: Arc::new(LocalFs::new(root.path("root")).unwrap()),
        key: ed25519_public(&keys.public()),
        user: user_name(),
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let listening = server::listen_on(
        Arc::new(server::Config::new(HostKey::generate().unwrap())),
        listener,
        Factory(conn),
    )
    .unwrap();
    OurServer {
        port,
        task: tokio::spawn(listening),
        root,
        keys,
    }
}

impl OurServer {
    fn served(&self, name: &str) -> PathBuf {
        self.root.path("root").join(name)
    }

    fn ssh_options(&self) -> Vec<String> {
        [
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "PasswordAuthentication=no",
            "-o",
            "LogLevel=ERROR",
            "-i",
            self.keys.private().to_str().unwrap(),
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    /// Runs `sftp` with a batch of commands; returns its output.
    async fn sftp(&self, batch: &str) -> std::process::Output {
        let file = self.root.path("batch.txt");
        fs::write(&file, batch).unwrap();
        let output = timeout(
            Duration::from_secs(60),
            Command::new("sftp")
                .arg("-P")
                .arg(self.port.to_string())
                .args(self.ssh_options())
                .arg("-b")
                .arg(&file)
                .arg(format!("{}@127.0.0.1", user_name()))
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .expect("sftp timed out")
        .expect("cannot run sftp");
        let text = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "sftp failed:\n{text}");
        output
    }
}

impl Drop for OurServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
#[ignore = "needs OpenSSH's ssh-keygen and sftp"]
async fn openssh_sftp_batch_against_our_server() {
    let srv = our_server().await;
    let work = Tmp::new();
    let big = random_bytes(4 * 1024 * 1024 + 17, 21);
    fs::write(work.path("big.bin"), &big).unwrap();
    fs::write(srv.served("hello.txt"), "hello from the share\n").unwrap();
    fs::create_dir(work.path("tree")).unwrap();
    fs::create_dir(work.path("tree/inner")).unwrap();
    fs::write(work.path("tree/a.txt"), "a").unwrap();
    fs::write(work.path("tree/inner/b.txt"), "b").unwrap();

    let batch = format!(
        "pwd\n\
         ls -l\n\
         get hello.txt {w}/hello.copy\n\
         put {w}/big.bin /big.bin\n\
         get /big.bin {w}/big.back\n\
         mkdir dir\n\
         rename big.bin dir/moved.bin\n\
         ln -s hello.txt link\n\
         chmod 600 hello.txt\n\
         df\n\
         put -r {w}/tree /tree\n\
         get -r /tree {w}/tree.back\n\
         ls -l dir\n\
         rm link\n\
         rename hello.txt hello2.txt\n\
         version\n\
         bye\n",
        w = work.0.display()
    );
    let output = srv.sftp(&batch).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Remote working directory: /"), "{stdout}");
    assert!(stdout.contains("hello.txt"), "{stdout}");
    assert!(stdout.contains("SFTP protocol version 3"), "{stdout}");
    assert!(
        stdout.contains("Size") && stdout.contains("Avail"),
        "df output: {stdout}"
    );

    assert_eq!(
        fs::read(work.path("hello.copy")).unwrap(),
        b"hello from the share\n"
    );
    assert_eq!(fs::read(work.path("big.back")).unwrap(), big);
    assert_eq!(fs::read(srv.served("dir/moved.bin")).unwrap(), big);
    assert!(!srv.served("big.bin").exists());
    assert_eq!(
        fs::metadata(srv.served("hello2.txt")).unwrap().mode() & 0o777,
        0o600
    );
    assert!(
        fs::symlink_metadata(srv.served("link")).is_err(),
        "link was removed"
    );
    assert_eq!(fs::read(srv.served("tree/a.txt")).unwrap(), b"a");
    assert_eq!(fs::read(srv.served("tree/inner/b.txt")).unwrap(), b"b");
    assert_eq!(fs::read(work.path("tree.back/inner/b.txt")).unwrap(), b"b");
}

#[tokio::test]
#[ignore = "needs OpenSSH's ssh-keygen and sftp"]
async fn openssh_sftp_resume_and_symlinks_against_our_server() {
    let srv = our_server().await;
    let work = Tmp::new();
    let data = random_bytes(3 * 1024 * 1024, 22);
    fs::write(srv.served("src.bin"), &data).unwrap();
    // A partial local copy is completed with `reget`, which reads from the
    // middle of the file.
    fs::write(work.path("part.bin"), &data[..1_000_000]).unwrap();
    // A partial remote copy is completed with `reput`.
    fs::write(srv.served("up.bin"), &data[..777_777]).unwrap();
    fs::write(work.path("up.bin"), &data).unwrap();
    srv.sftp(&format!(
        "reget src.bin {w}/part.bin\nreput {w}/up.bin up.bin\n\
         ln -s src.bin alias\nln -s /src.bin absolute\n\
         get alias {w}/via-alias\nget absolute {w}/via-absolute\nbye\n",
        w = work.0.display()
    ))
    .await;
    assert_eq!(fs::read(work.path("part.bin")).unwrap(), data);
    assert_eq!(fs::read(srv.served("up.bin")).unwrap(), data);
    assert_eq!(fs::read(work.path("via-alias")).unwrap(), data);
    assert_eq!(fs::read(work.path("via-absolute")).unwrap(), data);
    // The link text is what the client sent, in OpenSSH's argument order.
    assert_eq!(
        fs::read_link(srv.served("alias")).unwrap(),
        Path::new("src.bin")
    );
    assert_eq!(
        fs::read_link(srv.served("absolute")).unwrap(),
        Path::new("/src.bin")
    );
}

#[tokio::test]
#[ignore = "needs OpenSSH's ssh-keygen and sftp"]
async fn openssh_sftp_cannot_leave_the_served_directory() {
    let srv = our_server().await;
    fs::write(srv.root.path("outside.txt"), "OUTSIDE").unwrap();
    let work = Tmp::new();
    // Each of these must fail, and the run goes on thanks to the `-` prefix.
    srv.sftp(&format!(
        "-get ../outside.txt {w}/a\n-get /../outside.txt {w}/b\n\
         -get /etc/hostname {w}/c\n-put {w}/nothing /../planted.txt\nbye\n",
        w = work.0.display()
    ))
    .await;
    for name in ["a", "b", "c"] {
        assert!(!work.path(name).exists(), "{name} was fetched");
    }
    assert!(!srv.root.path("planted.txt").exists());
    // The link target is a path of the host, and the server reads it as a
    // path of the served tree.
    symlink(srv.root.path("outside.txt"), srv.served("escape")).unwrap();
    srv.sftp(&format!("-get escape {w}/d\nbye\n", w = work.0.display()))
        .await;
    assert!(!work.path("d").exists(), "followed a link out of the tree");
}

#[tokio::test]
#[ignore = "needs OpenSSH's ssh-keygen and scp"]
async fn openssh_scp_uses_the_sftp_subsystem_of_our_server() {
    let srv = our_server().await;
    let work = Tmp::new();
    let data = random_bytes(2 * 1024 * 1024, 23);
    fs::write(work.path("up.bin"), &data).unwrap();
    let target = format!("{}@127.0.0.1", user_name());
    let srv = &srv;
    let run = |args: Vec<String>| async move {
        let out = timeout(
            Duration::from_secs(60),
            Command::new("scp")
                .arg("-s")
                .arg("-P")
                .arg(srv.port.to_string())
                .args(srv.ssh_options())
                .args(args)
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .expect("scp timed out")
        .expect("cannot run scp");
        assert!(
            out.status.success(),
            "scp failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(vec![
        work.path("up.bin").to_str().unwrap().to_owned(),
        format!("{target}:/up.bin"),
    ])
    .await;
    assert_eq!(fs::read(srv.served("up.bin")).unwrap(), data);
    run(vec![
        format!("{target}:/up.bin"),
        work.path("down.bin").to_str().unwrap().to_owned(),
    ])
    .await;
    assert_eq!(fs::read(work.path("down.bin")).unwrap(), data);
}
