//! The SFTP client.
//!
//! [`Session`] runs the protocol over any pair of async byte streams. It
//! matches replies to requests by id, so any number of requests can be in
//! flight at once from any number of tasks, and replies may arrive in any
//! order.
//!
//! # Cancellation and timeouts
//!
//! Every operation is cancel safe. Dropping the future of a request that has
//! already been sent does not stop the server from running it, and its
//! request id stays reserved until the reply arrives, so a late reply can
//! never be mistaken for the answer to a newer request.
//!
//! The session sets no timeouts of its own. Wrap operations in
//! `tokio::time::timeout` where a bound is needed. A request that never
//! receives a reply keeps one of the [`Config::max_in_flight`] slots until
//! the connection ends.

mod dir;
mod ext;
mod file;
pub mod fs;
mod ops;
mod session;

pub use crate::extensions::{Limits, StatVfs};
pub use dir::Dir;
pub use file::{File, OpenOptions};
pub use session::{Config, Handle, Session};

/// Names of the OpenSSH extensions with typed methods, for
/// [`Session::supports`].
pub use crate::extensions;
