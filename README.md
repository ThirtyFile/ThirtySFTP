# ThirtySFTP

[![CI](https://github.com/thirtyfile/ThirtySFTP/actions/workflows/ci.yml/badge.svg)](https://github.com/thirtyfile/ThirtySFTP/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/thirtysftp.svg)](https://crates.io/crates/thirtysftp)
[![docs.rs](https://docs.rs/thirtysftp/badge.svg)](https://docs.rs/thirtysftp)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](https://github.com/thirtyfile/ThirtySFTP/blob/main/LICENSE)

**English** | [简体中文](https://github.com/thirtyfile/ThirtySFTP/blob/main/README.zh-CN.md) | [繁體中文](https://github.com/thirtyfile/ThirtySFTP/blob/main/README.zh-TW.md)

An asynchronous SFTP client and server library for Rust, built on Tokio. It
is an independent implementation. The protocol core works on any
`AsyncRead + AsyncWrite` byte stream, and an optional integration runs it over
[ThirtySSH](https://github.com/thirtyfile/ThirtySSH) channels.

> **Status:** pre-1.0. The API may change between minor versions. Read
> [SECURITY.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/SECURITY.md) before serving files to people you do not trust.

## Features

- SFTP protocol version 3, the version OpenSSH speaks, and these OpenSSH
  extensions: `posix-rename`, `statvfs`, `fstatvfs`, `hardlink`, `fsync`,
  `lsetstat`, `limits`, `expand-path`, `copy-data`, `home-directory` and
  `users-groups-by-id`
- **Client:** a low-level request interface (`Session`) and a `tokio::fs`-like
  one (`File`, `Dir`, `client::fs`). Many requests in flight, replies matched by
  id in any order, large files read and written in parallel, short reads made
  up, and `AsyncRead`/`AsyncWrite`/`AsyncSeek` on remote files
- **Server:** the protocol loop over a `Filesystem` trait you implement, with
  limits on packet size, handles, bytes per request, directory entries and
  requests in flight. Included file systems: `LocalFs`, which serves one local
  directory and keeps clients inside it, and `MemoryFs`
- Works with OpenSSH: tested against its `sftp-server`, `sshd`, `sftp` and
  `scp` ([docs/INTEROP.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/docs/INTEROP.md))
- No `unsafe` code, no panics on data from the peer, and about the speed of
  OpenSSH's own client on loopback
  ([docs/PERFORMANCE.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/docs/PERFORMANCE.md))

Not included: SFTP versions 4 to 6, which almost nothing deploys; the SCP
protocol; synchronisation or resumable-transfer scheduling.

## Cargo features

| Feature | Default | Purpose |
|---|---|---|
| `client` | yes | The SFTP client |
| `server` | yes | The SFTP server and `LocalFs` |
| `thirtyssh` | yes | Run over ThirtySSH channels (`thirtysftp::ssh`) |

The minimum supported Rust version is 1.89.

## Client

Open an SFTP session on an authenticated ThirtySSH connection and use it like
`tokio::fs`:

```rust,no_run
use thirtysftp::client::{Config, fs};
use thirtyssh::channel::driver::Handle;

async fn upload_and_list(ssh: &Handle) -> Result<(), thirtysftp::Error> {
    let sftp = thirtysftp::ssh::connect(ssh, Config::default()).await?;

    fs::write(&sftp, "/upload/report.txt", "hello").await?;
    assert_eq!(fs::read(&sftp, "/upload/report.txt").await?, b"hello");

    let mut dir = fs::read_dir(&sftp, "/upload").await?;
    while let Some(entry) = dir.next_entry().await? {
        println!("{}", String::from_utf8_lossy(&entry.name));
    }
    Ok(())
}
```

Over any other byte stream, `Session::connect(reader, writer, Config::default())`
does the same work. Operations have no time limit of their own; bound them with
`tokio::time::timeout`, which is safe to use on all of them.

## Server

Serve a directory to one connected client, on any pair of streams. The client
cannot reach anything outside the directory:

```rust,no_run
use std::sync::Arc;

use thirtysftp::server::{LocalFs, ServerConfig, serve};
use tokio::io::{AsyncRead, AsyncWrite};

async fn serve_directory(
    reader: impl AsyncRead + Unpin + Send,
    writer: impl AsyncWrite + Unpin + Send + 'static,
) -> Result<(), thirtysftp::Error> {
    let files = Arc::new(LocalFs::new("/srv/share")?.read_only(true));
    serve(reader, writer, files, ServerConfig::default()).await
}
```

To run it as an SSH subsystem, call `thirtysftp::ssh::accept_session` from a
ThirtySSH `channel_open` callback. The
[`sftp_server`](https://github.com/thirtyfile/ThirtySFTP/blob/main/crates/thirtysftp/examples/sftp_server.rs) example is a
complete server that OpenSSH's `sftp` can connect to, and
[`sftp_stdio`](https://github.com/thirtyfile/ThirtySFTP/blob/main/crates/thirtysftp/examples/sftp_stdio.rs) serves SFTP on standard
input and output like `sftp-server`.

## Performance

On loopback, with a 128 MiB file, this crate's client moves about 440-490 MiB/s
over SSH to an OpenSSH `sshd`, which is what OpenSSH's own `sftp` does on the
same server (about 420-480 MiB/s). The defaults (64 requests in flight; 32 KiB
requests, or up to 128 KiB when the server states its limits) come from the
measurements in
[docs/PERFORMANCE.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/docs/PERFORMANCE.md),
which also gives the method and the limits of the numbers. Benchmarks live in
the separate `thirtysftp-bench` crate.

## More

- [ARCHITECTURE.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/docs/ARCHITECTURE.md): how the crate is put together
- [SECURITY.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/SECURITY.md): threat model, limits and known limitations
- [CONTRIBUTING.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/CONTRIBUTING.md) and [CHANGELOG.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/CHANGELOG.md)

## License

Apache-2.0. See [LICENSE](https://github.com/thirtyfile/ThirtySFTP/blob/main/LICENSE).
