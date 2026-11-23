//! SFTP over [ThirtySSH](https://docs.rs/thirtyssh) channels.
//!
//! The protocol core works on any byte stream; this module is the glue that
//! gets one out of an SSH session channel, and the other way round.
//!
//! - **Client:** [`connect`] opens a session channel on an authenticated
//!   connection, asks for the `sftp` subsystem and starts a client `Session`
//!   on the channel.
//! - **Server:** [`accept_session`] is for a [`channel_open`](thirtyssh::session::Handler::channel_open)
//!   callback. It accepts a session channel and serves SFTP on it in a task of
//!   its own. [`spawn_session`] is the same and hands back the task, and
//!   [`serve_channel`] serves a channel you accepted yourself.
//!
//! # Behaviour of the SSH layer that matters here
//!
//! - A handler callback runs to completion before the next one starts, so
//!   [`accept_session`] only accepts the channel and hands the connection to
//!   a spawned task; a long SFTP session never blocks the SSH connection.
//! - Once a channel is turned into a byte stream, channel requests that
//!   arrive later are not seen, and ones that want a reply are refused. The
//!   subsystem request is therefore handled before the conversion.
//! - SFTP packets span several channel data packets (at most 32 KiB each);
//!   the packet reader reassembles them.
//! - The channel's receive window is ThirtySSH's default, 2 MiB, which is also
//!   the most ThirtySSH allows a channel that is read as a stream. On a link
//!   with a long round trip this, not the SFTP request size, bounds a transfer
//!   to about one window per round trip.
//! - When an SFTP session ends the server sends `exit-status` (0 for a clean
//!   end, 1 for an error) before it closes the channel, so a peer can tell an
//!   orderly end from a broken connection.
//!
//! # Example: the server side
//!
//! ```no_run
//! # fn main() {}
//! # #[cfg(feature = "server")]
//! # mod example {
//! use std::{io, sync::Arc};
//! use thirtyssh::{server, session};
//! use thirtysftp::server::{LocalFs, ServerConfig};
//!
//! struct Connection {
//!     fs: Arc<LocalFs>,
//! }
//!
//! impl server::AuthPolicy for Connection {
//!     async fn password(&mut self, user: &str, password: &[u8]) -> io::Result<bool> {
//!         Ok(user == "sftp" && password == b"correct horse") // replace with real checks
//!     }
//! }
//!
//! impl session::Handler for Connection {
//!     async fn channel_open(
//!         &mut self,
//!         request: session::IncomingChannel,
//!         _session: &session::Context,
//!     ) -> io::Result<()> {
//!         thirtysftp::ssh::accept_session(request, self.fs.clone(), ServerConfig::default()).await
//!     }
//! }
//! # }
//! ```
//!
//! # Example: the client side
//!
//! ```no_run
//! # fn main() {}
//! # #[cfg(feature = "client")]
//! # mod example {
//! # async fn demo(handle: thirtyssh::channel::driver::Handle) -> Result<(), thirtysftp::Error> {
//! use thirtysftp::client::{Config, fs};
//!
//! let sftp = thirtysftp::ssh::connect(&handle, Config::default()).await?;
//! let data = fs::read(&sftp, "/etc/hostname").await?;
//! # let _ = data;
//! # Ok(())
//! # }
//! # }
//! ```

// Counts as a use of the dependency when neither `client` nor `server` is on.
use thirtyssh as _;

#[cfg(feature = "server")]
use std::io;
#[cfg(feature = "server")]
use std::sync::Arc;

#[cfg(feature = "server")]
use thirtyssh::channel::Channel;
#[cfg(feature = "client")]
use thirtyssh::channel::driver::Handle;
#[cfg(any(feature = "client", feature = "server"))]
use thirtyssh::channel::{DEFAULT_WINDOW, Message};
#[cfg(feature = "server")]
use tokio::task::JoinHandle;

#[cfg(feature = "server")]
use thirtyssh::exec::Exit;
#[cfg(any(feature = "client", feature = "server"))]
use thirtyssh::session::Request;
#[cfg(feature = "server")]
use thirtyssh::session::{IncomingChannel, OpenType};

#[cfg(feature = "client")]
use crate::client::{Config, Session};
#[cfg(feature = "client")]
use crate::error::Error;
#[cfg(feature = "server")]
use crate::server::{Filesystem, ServerConfig};

/// The subsystem name SFTP is requested under.
pub const SUBSYSTEM: &str = "sftp";

/// Opens an SFTP session on an authenticated SSH connection.
///
/// Opens a `session` channel on `handle`, requests the `sftp` subsystem and
/// runs the SFTP handshake on the channel. Fails if the server refuses the
/// channel or the subsystem. The call waits on the server without a time
/// limit; wrap it in `tokio::time::timeout` to bound it.
///
/// Take the handle from the SSH session, for example `client.start()?.handle()`.
/// The SFTP session lives as long as the SSH connection does, and ends before
/// it if it is closed.
#[cfg(feature = "client")]
pub async fn connect(handle: &Handle, config: Config) -> Result<Session, Error> {
    let mut channel = handle.open_channel("session", DEFAULT_WINDOW, &[]).await?;
    channel.request(Request::Subsystem(SUBSYSTEM), true).await?;
    loop {
        match channel.recv().await {
            Some(Message::Reply { success: true }) => break,
            Some(Message::Reply { success: false }) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "the server refused the sftp subsystem",
                )
                .into());
            }
            // Requests that need an answer are not ours to serve.
            Some(Message::Request {
                want_reply: true, ..
            }) => channel.reply(false).await?,
            Some(Message::Request { .. } | Message::WindowAdjusted { .. }) => {}
            Some(Message::Closed) | None => return Err(Error::Disconnected),
            Some(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "unexpected data before the sftp subsystem started",
                )
                .into());
            }
        }
    }
    let (reader, writer) = tokio::io::split(channel.into_io());
    Session::connect(reader, writer, config).await
}

/// Accepts a channel-open request for an SFTP session and serves it in a new
/// task.
///
/// For use in [`channel_open`](thirtyssh::session::Handler::channel_open).
/// A request for anything but a `session` channel is refused (as
/// administratively prohibited), so handle other kinds before calling this.
/// The callback returns as soon as the channel is accepted. The task ends when
/// the client closes the SFTP session or the SSH connection goes away; any
/// failure there is the client's to see, as an `exit-status` of 1.
#[cfg(feature = "server")]
pub async fn accept_session<F: Filesystem>(
    request: IncomingChannel,
    fs: Arc<F>,
    config: ServerConfig,
) -> io::Result<()> {
    spawn_session(request, fs, config).await.map(drop)
}

/// Like [`accept_session`], returning the task that serves the session.
///
/// The task's result is the outcome of the SFTP session. Keep the handle to
/// wait for it, to log it, or to end the session early with `abort`; dropping
/// the handle leaves the session running. `None` means the request was not
/// for a `session` channel and was refused.
#[cfg(feature = "server")]
pub async fn spawn_session<F: Filesystem>(
    request: IncomingChannel,
    fs: Arc<F>,
    config: ServerConfig,
) -> io::Result<Option<JoinHandle<io::Result<()>>>> {
    if !matches!(request.open_type()?, OpenType::Session) {
        request.refuse(1).await?;
        return Ok(None);
    }
    let channel = request.accept(DEFAULT_WINDOW, &[]).await?;
    Ok(Some(tokio::spawn(serve_channel(channel, fs, config))))
}

/// Serves SFTP on an accepted session channel.
///
/// Waits for the `sftp` subsystem request and answers it, refusing any other
/// request that asks for a reply, then runs [`serve`](crate::server::serve)
/// on the channel. When that ends it sends `exit-status` (0 if the client
/// ended the session, 1 if it failed), then end-of-file, then closes the
/// channel. Returns the outcome of the SFTP session. A channel that closes
/// before asking for the subsystem is not an error.
#[cfg(feature = "server")]
pub async fn serve_channel<F: Filesystem>(
    mut channel: Channel,
    fs: Arc<F>,
    config: ServerConfig,
) -> io::Result<()> {
    loop {
        match channel.recv().await {
            Some(Message::Request {
                name,
                want_reply,
                fields,
            }) => {
                let wanted = matches!(
                    Request::decode(&name, &fields),
                    Ok(Request::Subsystem(SUBSYSTEM))
                );
                if want_reply {
                    channel.reply(wanted).await?;
                }
                if wanted {
                    break;
                }
            }
            Some(Message::Eof | Message::WindowAdjusted { .. } | Message::Reply { .. }) => {}
            Some(Message::Closed) | None => return Ok(()),
            Some(_) => {
                channel.close().await?;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "data on the channel before the sftp subsystem started",
                ));
            }
        }
    }
    let sender = channel.sender();
    let (reader, writer) = tokio::io::split(channel.into_io());
    let result = crate::server::serve(reader, writer, fs, config).await;
    // `serve` has shut down the write side. The report goes before the close
    // so the peer can tell an orderly end from a lost connection.
    let status = u32::from(result.is_err());
    let _ = sender
        .request(Request::Exit(Exit::Status(status)), false)
        .await;
    let _ = sender.eof().await;
    let _ = sender.close().await;
    result.map_err(io::Error::from)
}
