//! SFTP over a real SSH connection between ThirtySSH's client and server.
#![cfg(all(feature = "thirtyssh", feature = "client", feature = "server"))]

mod common;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use common::random_bytes;
use thirtysftp::Error;
use thirtysftp::client::{Config, fs};
use thirtysftp::server::{MemoryFs, ServerConfig};
use thirtysftp::ssh;
use thirtysftp::wire::{ClientPacket, Encode, Hello};
use thirtyssh::channel::{DEFAULT_WINDOW, Message, driver::Handle};
use thirtyssh::exec::Exit;
use thirtyssh::keys::{Algorithm, Credential};
use thirtyssh::session::{self, Context, IncomingChannel, Request};
use thirtyssh::transport::HostKey;
use thirtyssh::{client, server};
use tokio::io::duplex;
use tokio::time::{sleep, timeout};

const USER: &str = "sftp";
const PASSWORD: &str = "test password";

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Serve,
    /// Accept the session channel and refuse every request on it.
    RefuseSubsystem,
}

struct Srv {
    fs: Arc<MemoryFs>,
    config: ServerConfig,
    mode: Mode,
}

impl server::AuthPolicy for Srv {
    async fn password(&mut self, user: &str, password: &[u8]) -> io::Result<bool> {
        Ok(user == USER && password == PASSWORD.as_bytes())
    }
}

impl session::Handler for Srv {
    async fn channel_open(&mut self, request: IncomingChannel, _: &Context) -> io::Result<()> {
        if self.mode == Mode::RefuseSubsystem {
            let mut channel = request.accept(DEFAULT_WINDOW, &[]).await?;
            tokio::spawn(async move {
                while let Some(message) = channel.recv().await {
                    if let Message::Request {
                        want_reply: true, ..
                    } = message
                    {
                        let _ = channel.reply(false).await;
                    }
                }
            });
            return Ok(());
        }
        ssh::accept_session(request, self.fs.clone(), self.config.clone()).await
    }
}

struct Cli {
    pin: [u8; 32],
}

impl client::Handler for Cli {
    async fn verify_host(&mut self, host: &Credential, _: &Algorithm) -> io::Result<bool> {
        Ok(host.ed25519().is_some_and(|key| key == self.pin))
    }
}

impl session::Handler for Cli {}

/// Both ends of an authenticated SSH connection.
struct Link {
    client: session::Session,
    server: session::Session,
    fs: Arc<MemoryFs>,
}

impl Link {
    fn client(&self) -> Handle {
        self.client.handle()
    }

    fn server(&self) -> Handle {
        self.server.handle()
    }
}

// A running SSH session is itself a future; it is kept, not awaited.
#[allow(clippy::async_yields_async)]
async fn link_with(mode: Mode, config: ServerConfig) -> Link {
    let fs = Arc::new(MemoryFs::new());
    let host = HostKey::generate().unwrap();
    let pin = host.public_key();
    let mut server_config = server::Config::new(host);
    server_config.authentication.rejection_delay = Duration::ZERO;
    let (a, b) = duplex(64 * 1024);
    let handler = Srv {
        fs: fs.clone(),
        config,
        mode,
    };
    let (client, server) = tokio::join!(
        async {
            let mut client =
                client::connect_over(Arc::new(client::Config::default()), a, Cli { pin })
                    .await
                    .unwrap();
            assert!(
                client
                    .authenticate(USER, client::Method::Password(PASSWORD))
                    .await
                    .unwrap()
                    .is_accepted()
            );
            client.start().unwrap()
        },
        server::accept_over(Arc::new(server_config), b, None, handler),
    );
    Link {
        client,
        server: server.unwrap(),
        fs,
    }
}

async fn link() -> Link {
    link_with(Mode::Serve, ServerConfig::default()).await
}

fn init_frame() -> Vec<u8> {
    let mut out = BytesMut::new();
    ClientPacket::Init(Hello {
        version: 3,
        extensions: vec![],
    })
    .encode_frame(&mut out)
    .unwrap();
    out.to_vec()
}

#[tokio::test]
async fn files_move_both_ways_over_ssh() {
    timeout(Duration::from_secs(60), async {
        let link = link().await;
        let sftp = ssh::connect(&link.client(), Config::default())
            .await
            .unwrap();
        // The server's limits arrive through the handshake.
        assert!(sftp.known_limits().is_some());

        let data = random_bytes(3 * 1024 * 1024, 1);
        fs::write(&sftp, "/blob", data.clone()).await.unwrap();
        assert_eq!(link.fs.file_contents("/blob").unwrap(), data);
        assert_eq!(fs::read(&sftp, "/blob").await.unwrap(), data);

        fs::create_dir_all(&sftp, "/a/b").await.unwrap();
        fs::write(&sftp, "/a/b/f", &b"hello"[..]).await.unwrap();
        let mut dir = fs::read_dir(&sftp, "/a/b").await.unwrap();
        let entry = dir.next_entry().await.unwrap().unwrap();
        assert_eq!(entry.name, "f");
        assert!(dir.next_entry().await.unwrap().is_none());
        assert_eq!(
            sftp.stat("/missing").await.unwrap_err().status_code(),
            Some(thirtysftp::wire::StatusCode::NoSuchFile)
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_transfer_survives_rekeying_in_both_directions() {
    timeout(Duration::from_secs(120), async {
        let link = link().await;
        let sftp = ssh::connect(&link.client(), Config::default())
            .await
            .unwrap();
        let data = random_bytes(8 * 1024 * 1024, 2);
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rekeys = {
            let (done, client, server) = (done.clone(), link.client(), link.server());
            tokio::spawn(async move {
                let mut count = 0;
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    client.rekey().await.unwrap();
                    sleep(Duration::from_millis(15)).await;
                    server.rekey().await.unwrap();
                    sleep(Duration::from_millis(15)).await;
                    count += 2;
                }
                count
            })
        };
        fs::write(&sftp, "/big", data.clone()).await.unwrap();
        assert_eq!(fs::read(&sftp, "/big").await.unwrap(), data);
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let count = rekeys.await.unwrap();
        eprintln!("{count} key exchanges during the transfer");
        assert!(count >= 2);
        assert_eq!(link.fs.file_contents("/big").unwrap(), data);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn several_sftp_sessions_share_one_connection() {
    timeout(Duration::from_secs(120), async {
        let link = link().await;
        let handle = link.client();
        let mut tasks = Vec::new();
        for i in 0..4u64 {
            let handle = handle.clone();
            tasks.push(tokio::spawn(async move {
                let sftp = ssh::connect(&handle, Config::default()).await.unwrap();
                let data = random_bytes(1_000_000, i);
                let path = format!("/s{i}");
                fs::write(&sftp, &path, data.clone()).await.unwrap();
                assert_eq!(fs::read(&sftp, &path).await.unwrap(), data);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn the_session_ends_with_exit_status_zero_when_the_client_is_done() {
    timeout(Duration::from_secs(30), async {
        let link = link().await;
        let mut channel = link
            .client()
            .open_channel("session", DEFAULT_WINDOW, &[])
            .await
            .unwrap();
        // Requests that are not the sftp subsystem are refused, and the
        // channel stays usable.
        channel
            .request(Request::Subsystem("netconf"), true)
            .await
            .unwrap();
        assert_eq!(
            channel.recv().await,
            Some(Message::Reply { success: false })
        );
        channel
            .raw_request("env", b"\0\0\0\x01A\0\0\0\x01B", true)
            .await
            .unwrap();
        assert_eq!(
            channel.recv().await,
            Some(Message::Reply { success: false })
        );
        channel
            .request(Request::Subsystem("sftp"), true)
            .await
            .unwrap();
        assert_eq!(channel.recv().await, Some(Message::Reply { success: true }));

        channel.send(init_frame()).await.unwrap();
        let mut reply = Vec::new();
        while reply.len() < 4 + 5 {
            match channel.recv().await {
                Some(Message::Data(data)) => reply.extend_from_slice(&data),
                other => panic!("{other:?}"),
            }
        }
        // VERSION 3 first, then the extensions the server announces.
        assert_eq!(&reply[4..9], &[2, 0, 0, 0, 3]);

        channel.eof().await.unwrap();
        let mut status = None;
        let mut closed = false;
        while let Some(message) = channel.recv().await {
            match message {
                Message::Request { name, fields, .. } => {
                    assert!(!closed, "report after close");
                    if let Ok(Request::Exit(exit)) = Request::decode(&name, &fields) {
                        status = Some(exit);
                    }
                }
                Message::Closed => {
                    closed = true;
                    break;
                }
                Message::Data(_) | Message::Eof | Message::WindowAdjusted { .. } => {}
                other => panic!("{other:?}"),
            }
        }
        assert!(closed, "channel never closed");
        assert_eq!(status, Some(Exit::Status(0)));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_protocol_violation_ends_with_exit_status_one() {
    timeout(Duration::from_secs(30), async {
        let link = link().await;
        let mut channel = link
            .client()
            .open_channel("session", DEFAULT_WINDOW, &[])
            .await
            .unwrap();
        channel
            .request(Request::Subsystem("sftp"), true)
            .await
            .unwrap();
        assert_eq!(channel.recv().await, Some(Message::Reply { success: true }));
        // Two handshakes: the second ends the session.
        let mut both = init_frame();
        both.extend(init_frame());
        channel.send(both).await.unwrap();
        let mut status = None;
        while let Some(message) = channel.recv().await {
            match message {
                Message::Request { name, fields, .. } => {
                    if let Ok(Request::Exit(exit)) = Request::decode(&name, &fields) {
                        status = Some(exit);
                    }
                }
                Message::Closed => break,
                _ => {}
            }
        }
        assert_eq!(status, Some(Exit::Status(1)));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn refusals_are_reported_to_the_client() {
    timeout(Duration::from_secs(30), async {
        // A server that accepts the channel but not the subsystem.
        let refusing = link_with(Mode::RefuseSubsystem, ServerConfig::default()).await;
        let err = ssh::connect(&refusing.client(), Config::default())
            .await
            .map(|_| ())
            .unwrap_err();
        match err {
            Error::Io(e) => assert_eq!(e.kind(), io::ErrorKind::Unsupported),
            other => panic!("{other:?}"),
        }

        // A channel that is not a session is refused outright.
        let link = link().await;
        let err = link
            .client()
            .open_channel("custom@example.test", 1024, &[])
            .await
            .map(|_| ())
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn losing_the_connection_ends_both_sides_cleanly() {
    timeout(Duration::from_secs(60), async {
        let link = link().await;
        let sftp = ssh::connect(&link.client(), Config::default())
            .await
            .unwrap();
        // Handler and server task each hold the file system while alive.
        assert!(Arc::strong_count(&link.fs) >= 3);
        let data = random_bytes(8 * 1024 * 1024, 3);
        let transfer = tokio::spawn({
            let sftp = sftp.clone();
            async move { fs::write(&sftp, "/cut", data).await }
        });
        // Cut the connection once the transfer is under way, without
        // assuming how long it takes.
        while !link.fs.exists("/cut") {
            tokio::task::yield_now().await;
        }
        let Link {
            client,
            server,
            fs: files,
        } = link;
        client.disconnect().await.unwrap();
        // The transfer fails instead of hanging, and a later call fails too.
        let result = timeout(Duration::from_secs(20), transfer)
            .await
            .unwrap()
            .unwrap();
        // It was cut in the middle unless this machine finished 8 MiB in the
        // time the disconnect took; either way it has not hung.
        if result.is_ok() {
            eprintln!("the transfer finished before the connection was cut");
        }
        assert!(
            timeout(Duration::from_secs(20), sftp.stat("/"))
                .await
                .unwrap()
                .is_err()
        );
        let _ = timeout(Duration::from_secs(20), server).await.unwrap();
        // Nothing of the connection is left holding the file system.
        for _ in 0..200 {
            if Arc::strong_count(&files) == 1 {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
        panic!("the server side still holds the file system");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn closing_the_sftp_session_leaves_the_connection_usable() {
    timeout(Duration::from_secs(60), async {
        let link = link().await;
        let first = ssh::connect(&link.client(), Config::default())
            .await
            .unwrap();
        fs::write(&first, "/x", &b"one"[..]).await.unwrap();
        first.close();
        drop(first);
        // The server task for the first session ends; a second one works.
        let second = ssh::connect(&link.client(), Config::default())
            .await
            .unwrap();
        assert_eq!(fs::read(&second, "/x").await.unwrap(), b"one");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn the_session_task_can_be_watched() {
    timeout(Duration::from_secs(60), async {
        // A server whose handler keeps the task of every session it starts.
        struct Watching {
            fs: Arc<MemoryFs>,
            tasks: tokio::sync::mpsc::UnboundedSender<tokio::task::JoinHandle<io::Result<()>>>,
        }
        impl server::AuthPolicy for Watching {
            async fn password(&mut self, user: &str, password: &[u8]) -> io::Result<bool> {
                Ok(user == USER && password == PASSWORD.as_bytes())
            }
        }
        impl session::Handler for Watching {
            async fn channel_open(
                &mut self,
                request: IncomingChannel,
                _: &Context,
            ) -> io::Result<()> {
                let task =
                    ssh::spawn_session(request, self.fs.clone(), ServerConfig::default()).await?;
                if let Some(task) = task {
                    let _ = self.tasks.send(task);
                }
                Ok(())
            }
        }

        let fs_ = Arc::new(MemoryFs::new());
        let (tasks_tx, mut tasks) = tokio::sync::mpsc::unbounded_channel();
        let host = HostKey::generate().unwrap();
        let pin = host.public_key();
        let mut server_config = server::Config::new(host);
        server_config.authentication.rejection_delay = Duration::ZERO;
        let (a, b) = duplex(64 * 1024);
        let (client, server) = tokio::join!(
            async {
                let mut client =
                    client::connect_over(Arc::new(client::Config::default()), a, Cli { pin })
                        .await
                        .unwrap();
                assert!(
                    client
                        .authenticate(USER, client::Method::Password(PASSWORD))
                        .await
                        .unwrap()
                        .is_accepted()
                );
                client
            },
            server::accept_over(
                Arc::new(server_config),
                b,
                None,
                Watching {
                    fs: fs_.clone(),
                    tasks: tasks_tx
                }
            ),
        );
        let client = client.start().unwrap();
        let _server = server.unwrap();

        let sftp = ssh::connect(&client.handle(), Config::default())
            .await
            .unwrap();
        let data = random_bytes(3 * 1024 * 1024, 5);
        fs::write(&sftp, "/w", &data).await.unwrap();
        assert_eq!(fs::read(&sftp, "/w").await.unwrap(), data);

        // The task ends, cleanly, when the client closes its session.
        let task = tasks.recv().await.unwrap();
        assert!(!task.is_finished());
        sftp.close();
        drop(sftp);
        task.await.unwrap().unwrap();

        // A channel that is not a session yields no task.
        assert!(
            client
                .handle()
                .open_channel("custom@example.test", 1024, &[])
                .await
                .is_err()
        );
        assert!(tasks.try_recv().is_err());
    })
    .await
    .unwrap();
}
