//! Asynchronous SFTP client and server library for Tokio.
//!
//! This crate implements SFTP protocol version 3, the version OpenSSH speaks,
//! and the OpenSSH extensions `posix-rename`, `statvfs`, `fstatvfs`,
//! `hardlink`, `fsync`, `lsetstat`, `limits`, `expand-path`, `copy-data`,
//! `home-directory` and `users-groups-by-id`. The protocol core works on any
//! `AsyncRead + AsyncWrite` byte stream; the [`ssh`] module runs it over
//! [ThirtySSH](https://docs.rs/thirtyssh) channels.
//!
//! # Modules
//!
//! - [`client`]: [`Session`](client::Session) (one request method per protocol
//!   operation), [`File`](client::File) and [`Dir`](client::Dir), and
//!   [`client::fs`], functions in the manner of `tokio::fs`.
//! - [`server`]: [`serve`](server::serve) runs the protocol over a
//!   [`Filesystem`](server::Filesystem) you implement, bounded by
//!   [`ServerConfig`](server::ServerConfig). [`LocalFs`](server::LocalFs)
//!   serves a local directory and keeps clients inside it;
//!   [`MemoryFs`](server::MemoryFs) lives in memory.
//! - [`ssh`]: glue to ThirtySSH.
//! - [`wire`] and [`codec`]: the packet encoding and framing the two sides are
//!   built on, for tools that need to speak the protocol directly.
//! - [`extensions`]: extension names and their data types.
//!
//! # Cargo features
//!
//! | Feature | Default | Purpose |
//! |---|---|---|
//! | `client` | yes | the client |
//! | `server` | yes | the server and `LocalFs` |
//! | `thirtyssh` | yes | the [`ssh`] module |
//!
//! # Client example
//!
//! ```no_run
//! # fn main() {}
//! # #[cfg(all(feature = "client", feature = "thirtyssh"))]
//! # mod example {
//! use thirtysftp::client::{Config, fs};
//! use thirtyssh::channel::driver::Handle;
//!
//! async fn upload_and_list(ssh: &Handle) -> Result<(), thirtysftp::Error> {
//!     let sftp = thirtysftp::ssh::connect(ssh, Config::default()).await?;
//!
//!     fs::write(&sftp, "/upload/report.txt", "hello").await?;
//!     assert_eq!(fs::read(&sftp, "/upload/report.txt").await?, b"hello");
//!
//!     let mut dir = fs::read_dir(&sftp, "/upload").await?;
//!     while let Some(entry) = dir.next_entry().await? {
//!         println!("{}", String::from_utf8_lossy(&entry.name));
//!     }
//!     Ok(())
//! }
//! # }
//! ```
//!
//! Operations have no time limit of their own; bound them with
//! `tokio::time::timeout`, which is safe to use on all of them.
//!
//! # Server example
//!
//! ```no_run
//! # fn main() {}
//! # #[cfg(all(feature = "server"))]
//! # mod example {
//! use std::sync::Arc;
//!
//! use thirtysftp::server::{LocalFs, ServerConfig, serve};
//! use tokio::io::{AsyncRead, AsyncWrite};
//!
//! async fn serve_directory(
//!     reader: impl AsyncRead + Unpin + Send,
//!     writer: impl AsyncWrite + Unpin + Send + 'static,
//! ) -> Result<(), thirtysftp::Error> {
//!     let files = Arc::new(LocalFs::new("/srv/share")?.read_only(true));
//!     serve(reader, writer, files, ServerConfig::default()).await
//! }
//! # }
//! ```
//!
//! # Safety and limits
//!
//! The crate forbids `unsafe` code. Nothing a peer sends can make it panic or
//! allocate from a length the peer states; the server's limits are all in
//! [`ServerConfig`](server::ServerConfig). See `SECURITY.md` in the repository
//! for the threat model and the limitations of `LocalFs`.

#[cfg(feature = "client")]
pub mod client;
pub mod codec;
mod error;
pub mod extensions;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "thirtyssh")]
pub mod ssh;
pub mod wire;

pub use error::{Error, ProtocolError, Status};

// The examples in the README are compiled as tests, so they cannot go stale.
#[cfg(all(doctest, feature = "client", feature = "server", feature = "thirtyssh"))]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;
