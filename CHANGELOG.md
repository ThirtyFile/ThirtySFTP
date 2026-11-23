# Changelog

All notable changes are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project aims to
follow [Semantic Versioning](https://semver.org/) once it reaches 1.0.

## [Unreleased]

### Added
- Project skeleton: workspace, lint settings, CI, release workflow and
  `cargo-deny` configuration.
- `wire`: encoding and decoding of every SFTP version 3 packet, file
  attributes and handshake, with byte-string paths and bounded counts.
- `codec`: `FrameReader` and `FrameWriter` for packets on a byte stream, with
  a length limit checked before any allocation and cancel-safe read and flush.
- `client`: `Session` with version negotiation, reply matching by request id,
  pipelining, an in-flight limit, and every version 3 operation as a typed
  method. Request ids stay reserved until their reply arrives, so a cancelled
  request can never cause a later reply to be misattributed.
- `client::File`, `OpenOptions`, `Dir` and the `client::fs` functions (a
  `tokio::fs`-like interface). Large reads and writes keep many requests in
  flight; a reply with less data than asked for is made up before anything
  later is used. `File` implements `AsyncRead`, `AsyncWrite` and `AsyncSeek`,
  with pipelined writes.
- Typed methods for the OpenSSH extensions `posix-rename`, `statvfs`,
  `fstatvfs`, `hardlink`, `fsync`, `lsetstat`, `limits`, `expand-path`,
  `copy-data`, `home-directory` and `users-groups-by-id`, and
  `Session::supports` to ask whether the server has one. The server's
  `limits` cap the request sizes of transfers.
- `server`: `serve` runs the protocol on any pair of byte streams over a
  user-implemented `Filesystem` trait, with `ServerConfig` limits for packet
  size, open handles, read and write length, directory entries per reply and
  requests in flight, all announced through `limits@openssh.com`. Requests run
  in their own tasks, in order per handle; a panicking request gets a failure
  status. `MemoryFs` is a complete in-memory implementation.
- `server::LocalFs`: serves a local directory, confined to it. On Unix paths
  are resolved against directory descriptors without following links (uses the
  `rustix` crate); on Windows reparse points and special names are refused.
  Has a read-only mode. `SECURITY.md` lists what it does and does not protect
  against.
- `ssh`: SFTP over ThirtySSH channels. `ssh::connect` opens an SFTP session on
  an SSH connection; `ssh::accept_session` serves one from a `channel_open`
  callback in a task of its own, and `ssh::serve_channel` serves a channel you
  accepted yourself. The server reports `exit-status` before it closes the
  channel. The `sftp_server` example serves a directory and works with
  OpenSSH's `sftp`.
- `statvfs@openssh.com` and `fstatvfs@openssh.com` are announced as version 2,
  which OpenSSH's client requires before it uses them.
- Interoperability tests with OpenSSH (`tests/openssh.rs`, `#[ignore]`d) and a
  CI job that runs them; `docs/INTEROP.md` lists what is covered. The
  `sftp_stdio` example serves SFTP on standard input and output.
- `thirtysftp-bench` (not published): throughput measurements and
  `compare.sh`, which times this client and OpenSSH's `sftp` against this server
  and `sshd`; the results and the method are in `docs/PERFORMANCE.md`.
- Defaults changed after measuring: `Config::chunk_size` is now an `Option`;
  unset, requests are 32 KiB, or as much as the server's `limits@openssh.com`
  allows up to 128 KiB. The server's default read and write length is 128 KiB
  (was 64 KiB).
- Documentation: `docs/ARCHITECTURE.md`; `SECURITY.md` now has the threat model
  and the limits of the client and the server; the README examples are
  compiled as tests; the crate documentation has an overview and both examples.
- `File`'s `AsyncRead` reads ahead: it keeps several requests in flight,
  doubling up to the pipeline depth, and recovers from short replies, so
  `tokio::io::copy` from a remote file runs at full speed without holding the
  file in memory.
- `Session::close`, and dropping the last `Session`, release the stream at
  once: a write stuck on a peer that has stopped reading is abandoned instead
  of holding the stream and a task indefinitely.
- `LocalFs` no longer offers `users-groups-by-id@openssh.com` unless
  `LocalFs::account_names(true)` is set, because the names are information
  about the host. The lookup reads each account file once per request instead
  of once per id.
- `ServerConfig::handshake_timeout` (60 s by default) and `idle_timeout` (off
  by default).
- `LocalFs`: set-user-id and set-group-id bits from clients are dropped unless
  `special_mode_bits(true)`; `max_operations` bounds the file system calls in
  progress (64 by default); on Linux a rename that must not replace is one
  atomic step; what is at a path is looked at before it is opened; an open
  directory listing on Windows no longer stops others from renaming or
  removing the directory.
- **Known gap on Windows `LocalFs`**, found by a new race test and documented
  in `SECURITY.md`: a local process that turns a directory of the served tree
  into a junction at the right moment can redirect a request. Operations now
  check again afterwards, which stops reads in practice but not creation.
- `ssh::spawn_session` returns the task that serves a session.
- `Error::Encode` replaces `ProtocolError::Encode`: a request too large for
  the protocol is not the peer's fault.
- `client::fs::write` takes anything that is bytes, not only `Bytes`.
- Fuzz targets (`fuzz/`, cargo-fuzz) for packet decoding and framing.
- `extensions`: extension names and the `Limits` and `StatVfs` types, shared by
  client and server.
- `Error` with the server's status code, protocol violations, I/O failures and
  connection loss; conversion to `io::Error`.
