//! The SFTP server.
//!
//! [`serve`] runs the protocol on any pair of async byte streams and exposes
//! the file system you give it through the [`Filesystem`] trait. It speaks
//! version 3 and dispatches the extensions the file system lists.
//!
//! # Limits
//!
//! Everything a client can make the server hold is bounded by
//! [`ServerConfig`]: the packet size (checked before a buffer is allocated),
//! open handles, bytes per read and write, directory entries per reply, and
//! requests in flight (the server stops reading when the last is reached).
//! The limits are announced to clients through `limits@openssh.com`.
//!
//! # Concurrency
//!
//! Each request runs in its own task. Requests on one handle run in the order
//! they arrived; all others run concurrently, so one slow call does not hold
//! up the rest. A request that panics gets a failure status instead of
//! leaving the client waiting.
//!
//! # Not covered here
//!
//! The server does not authenticate anyone: that is the job of the SSH layer
//! or whatever carries the streams. Policy such as read-only access or a
//! restricted root belongs in the [`Filesystem`] implementation.

mod config;
mod ext;
mod filesystem;
#[cfg(any(unix, windows))]
pub mod local;
mod longname;
pub mod memory;
mod reply;
mod serve;
mod table;

pub use config::ServerConfig;
pub use filesystem::{Filesystem, FsError};
#[cfg(any(unix, windows))]
pub use local::LocalFs;
pub use longname::format_long_name;
pub use memory::MemoryFs;
pub use serve::serve;
