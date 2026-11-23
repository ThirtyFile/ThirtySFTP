//! A complete SFTP server on top of ThirtySSH: one directory, one password.
//!
//! ```text
//! THIRTYSFTP_PASSWORD=secret cargo run --example sftp_server -- /path/to/share
//! sftp -P PORT sftp@127.0.0.1
//! ```
//!
//! The server listens on the loopback interface only, on the port given as
//! the second argument (default: any free port, which is printed). It uses a
//! host key made when it starts, so a client sees a new key every run; pass
//! `-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null` to `sftp`, or
//! use the `known_hosts` line the server prints.
//!
//! This is an example, not a hardened service: one shared password, no rate
//! limit beyond the library's defaults, no logging of requests.

use std::{env, io, net::Ipv4Addr, net::SocketAddr, sync::Arc};

use data_encoding::BASE64;
use thirtysftp::server::{LocalFs, ServerConfig};
use thirtyssh::{server, session, transport::HostKey};
use tokio::net::TcpListener;

const USER: &str = "sftp";
const ED25519_PREFIX: &[u8] = b"\0\0\0\x0bssh-ed25519\0\0\0\x20";

/// What one SSH connection needs: the file system to serve and the password.
#[derive(Clone)]
struct Connection {
    fs: Arc<LocalFs>,
    password: Arc<str>,
}

impl server::AuthPolicy for Connection {
    async fn password(&mut self, user: &str, password: &[u8]) -> io::Result<bool> {
        Ok(user == USER && password == self.password.as_bytes())
    }
}

impl session::Handler for Connection {
    async fn channel_open(
        &mut self,
        request: session::IncomingChannel,
        _session: &session::Context,
    ) -> io::Result<()> {
        // Accept the channel and hand it to a task; the callback must return.
        thirtysftp::ssh::accept_session(request, self.fs.clone(), ServerConfig::default()).await
    }
}

struct Factory(Connection);

impl server::Connections for Factory {
    type Handler = Connection;

    fn handler(&mut self, _peer: SocketAddr) -> Connection {
        self.0.clone()
    }

    fn failed(&mut self, error: io::Error) {
        eprintln!("connection ended: {error}");
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let usage = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: THIRTYSFTP_PASSWORD=... sftp_server DIRECTORY [PORT]",
        )
    };
    let mut args = env::args().skip(1);
    let root = args.next().ok_or_else(usage)?;
    let port: u16 = args
        .next()
        .map(|p| p.parse().map_err(|_| usage()))
        .transpose()?
        .unwrap_or(0);
    let password = env::var("THIRTYSFTP_PASSWORD").map_err(|_| usage())?;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let port = listener.local_addr()?.port();
    let host_key = HostKey::generate()?;
    let blob = [ED25519_PREFIX, &host_key.public_key()].concat();
    println!(
        "known_hosts line: [127.0.0.1]:{port} ssh-ed25519 {}",
        BASE64.encode(&blob)
    );
    println!("serving {root} on 127.0.0.1:{port} as user {USER}");

    let connection = Connection {
        fs: Arc::new(LocalFs::new(root)?),
        password: password.into(),
    };
    server::listen_on(
        Arc::new(server::Config::new(host_key)),
        listener,
        Factory(connection),
    )?
    .await
}
