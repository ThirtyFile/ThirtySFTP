# Architecture

One crate, `thirtysftp`, built from the bottom up. The lower layers know
nothing about the upper ones, and only the last one knows about SSH.

```
              ssh        (feature thirtyssh)  glue to ThirtySSH channels
    client            server                  (features client, server)
              codec                           packets on a byte stream
              wire                            packet encoding, pure
```

## `wire`: packets

Encoding and decoding of every SFTP version 3 packet, and of file attributes,
with no I/O. Decoding takes the payload of one packet and returns a typed value
or a `DecodeError`; encoding writes a whole frame, length prefix included.

- Paths, handles, messages and data are `bytes::Bytes`, because the protocol
  does not say file names are UTF-8. Decoded strings share the buffer of the
  packet they came from.
- Nothing is allocated from a length the peer states. A string must fit in the
  rest of the packet, and an entry count must fit in the bytes that could hold
  that many entries.
- A request type or reply type this crate does not know decodes to `Unknown`,
  keeping its id, so the server can answer it.
- Requests are strict about trailing bytes. A `STATUS` reply is accepted
  without its message and language fields, because some servers leave them out.

## `codec`: frames

`FrameReader` and `FrameWriter` read and write whole packets on a stream. The
length limit is checked as soon as the 4 length bytes are in, before any
buffer is sized for the packet. Both keep all progress inside the struct, so a
future that is dropped half way (in a `select!`, say) loses nothing: the reader
resumes from the bytes it has, and the writer from what it has not yet written.

## `client`

`Session` owns the connection. A background task reads replies and hands each
to the request it answers by id; another writes queued frames, several per
flush when they are waiting.

- **Ids and slots.** A request takes a slot (a semaphore permit, at most
  `max_in_flight`) and registers an id in a table before it is sent. Both are
  released when the *reply arrives*, not when the caller stops waiting. A
  cancelled request therefore keeps its id until the late reply comes, and a
  late reply can never be matched to a newer request.
- **Failure.** Any protocol violation or I/O error ends the session: every
  waiting request is woken with the error, the semaphore is closed, and later
  requests fail at once.
- **Transfers.** `File::read_range` and `write_all_at` keep a window of
  requests in flight. A reply with less data than asked for is made up by a new
  request placed ahead of everything sent after it, so the bytes arrive in
  order whatever the server does. Request sizes follow the server's
  `limits@openssh.com`. `File`'s `AsyncRead` reads ahead in the same way,
  with a window that starts at one request and doubles with every full
  reply, and `AsyncWrite` sends without waiting for replies, so streaming
  copies need no more memory than the window.
- **Ending.** When the session fails or is closed the writer stops between
  frames and shuts the stream down; if it is stuck in the middle of a frame
  because the peer is not reading, it gives the stream up instead.
- **Typed operations** (`ops`, `ext`) turn a request and its reply into a Rust
  call and check that the reply is the kind the request allows.

## `server`

`serve(reader, writer, Arc<impl Filesystem>, ServerConfig)` runs one
connection. The loop is in `serve.rs`; the limits in `config.rs`; the handle
table and the per-handle request order in `table.rs`; the reply guard in
`reply.rs`; the dispatch of extended requests in `ext.rs`.

- **Handles.** The `Filesystem` trait uses its own types for open files and
  directories. The loop keeps them in a table under opaque 8-byte counters that
  are never reused, so an implementation never sees handle bytes a client
  chose, and a stale handle cannot name a newer file.
- **Tasks.** Each request runs in a task of its own. Requests on the same
  handle are chained, each waiting for the one before it, in the order they
  arrived; everything else runs concurrently. A task that panics answers with a
  failure instead of leaving the client waiting.
- **Back pressure.** A semaphore of `max_pending` permits is taken before a
  packet is read and released when its reply has been written. When the limit
  is reached the loop stops reading.
- **File systems.** `MemoryFs` is a complete implementation for tests and as a
  model. `LocalFs` serves a directory of the disk: on Unix it resolves paths
  one component at a time against directory descriptors without following
  links, on Windows it refuses links and special names; see its module
  documentation and `SECURITY.md`.

## `ssh`

The only module that knows about ThirtySSH. `connect` opens a `session`
channel, requests the `sftp` subsystem, and turns the channel into a byte
stream for `Session`. `accept_session` accepts a channel in a `channel_open`
callback and serves it in a task, because ThirtySSH runs callbacks one at a
time. `serve_channel` waits for the subsystem request (which must be handled
before the channel becomes a stream) and sends `exit-status` before closing.

## Tests

| Where | What |
|---|---|
| `tests/wire.rs` | hand-written byte vectors, round trips of every packet, truncation, oversize and bad counts, seeded random and corrupted input |
| `tests/codec.rs` | framing: byte-at-a-time delivery, limits, cancellation, partial writes |
| `tests/client.rs` | the client against a scripted server: out-of-order replies, cancellation, id reuse, slot limits, failure |
| `tests/highlevel.rs`, `tests/ext.rs` | files, directories and extensions against an in-memory server that stores data |
| `tests/server.rs` | the server against the client and against raw packets: limits, handles, ordering, panics |
| `tests/local.rs` | `LocalFs` confinement and behaviour, including a race against a thread swapping a directory for a link |
| `tests/ssh.rs` | everything over a real ThirtySSH connection, with rekeying |
| `tests/openssh.rs` | OpenSSH's `sftp-server`, `sshd`, `sftp` and `scp` (`#[ignore]`; see `INTEROP.md`) |

Every fix comes with a test that fails without it. Random inputs come from a
fixed-seed generator, so a failure repeats. `fuzz/` holds cargo-fuzz targets
for packet decoding and framing (`cargo +nightly fuzz run decode`). Helpers
shared by the test files are in `tests/common`.

## Adding an extension

1. Add its name (and payload types, if any) to `extensions`.
2. Client: a method in `client/ext.rs` that checks the server announced it and
   builds and parses the payload.
3. Server: a method with a refusing default in the `Filesystem` trait, a
   branch in `server/serve.rs` that parses the request, and a line in
   `KNOWN`. A file system announces the extension by listing it in
   `Filesystem::extensions`.
4. Tests for both ends, and an entry in `docs/INTEROP.md` if OpenSSH has it.
