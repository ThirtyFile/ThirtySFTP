//! Serves SFTP on standard input and output, like OpenSSH's `sftp-server`.
//!
//! ```text
//! sftp_stdio DIRECTORY
//! ```
//!
//! Point an SSH server's `Subsystem sftp` line at it, or run it under any
//! program that gives it a pipe. The directory is the root the client sees.
//! There is no authentication here: whoever can start the process has the
//! access of the user running it.

use std::{env, io, sync::Arc};

use thirtysftp::server::{LocalFs, ServerConfig, serve};

#[tokio::main]
async fn main() -> io::Result<()> {
    let root = env::args().nth(1).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "usage: sftp_stdio DIRECTORY")
    })?;
    let fs = Arc::new(LocalFs::new(root)?);
    serve(
        tokio::io::stdin(),
        tokio::io::stdout(),
        fs,
        ServerConfig::default(),
    )
    .await
    .map_err(io::Error::from)
}
