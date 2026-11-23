# Security policy

## Reporting a vulnerability

Please do **not** open a public issue for security problems. Use GitHub private
vulnerability reporting (the "Security" tab, then "Report a vulnerability") on
this repository. Include the affected version or commit, a description, and a
reproduction if you have one.

If you find a way out of a `LocalFs` root, a way to make the client or server
allocate without a bound, or input from a peer that makes either panic, that is
a vulnerability: use this channel.

## Supported versions

Until 1.0 only the latest commit on the default branch receives fixes.

## Threat model

ThirtySFTP is a protocol library. It does not authenticate anyone: that is the
job of the SSH layer, or whatever carries the byte streams.

**What it defends against**

- *A hostile client against the server.* It can send any bytes. The server must
  not panic, must not hold more than its configured limits, must not let the
  client name a file system object the implementation did not hand out, and, with
  `LocalFs`, must not let the client reach outside the served directory.
- *A hostile server against the client.* It can send any bytes. The client must
  not panic, must not match a reply to the wrong request, and must not allocate
  from a length or count the server states.
- *A slow or stalled peer.* Memory stays bounded; time does not, see below.

**What it does not defend against**

- Anyone who can already run code in the process, or who controls the file
  system under the served directory (see `LocalFs` below).
- A server that is authorised to do what it does: a `Filesystem` implementation
  decides what each request means, including access control. `LocalFs` has a
  read-only mode and nothing else; there are no per-user rules.
- Traffic analysis, and everything the transport (SSH) is responsible for:
  confidentiality, integrity, authentication of both ends.

**Method.** Decoding is bounds-checked everywhere and tested against truncated,
oversize and corrupted packets and fixed-seed random input
(`tests/wire.rs`, `tests/codec.rs`). The library has `#![forbid(unsafe_code)]`;
the platform calls `LocalFs` needs go through the `rustix` crate on Unix and
the standard library on Windows. Every fix comes with a test that fails
without it.

## Limits

### Server (`ServerConfig`)

Everything a client can make the server hold is bounded, and every bound has a
default and can be changed:

| Limit | Default | Notes |
|---|---|---|
| Packet size | 256 KiB | checked from the length field, before a buffer is allocated; a longer packet ends the connection |
| Open handles | 256 | files and directories together |
| Bytes per read / write | 128 KiB | a longer read is answered short, a longer write is refused |
| Entries per directory reply | 100 | the rest are kept for the next request |
| Requests in flight | 128 | the server stops reading when it is reached; a reply counts until it is written |
| Time to the first packet | 60 s | `handshake_timeout`; `None` waits for ever |
| Idle time | none | `idle_timeout`: no request arriving and none being served |

The limits are announced to clients with `limits@openssh.com`. A handle is an
opaque counter that is never reused. Nothing a peer sends is trusted for a
length or a count: lengths are checked against the packet, counts against the
bytes that could hold them.

### Client (`client::Config`)

Reply packets are limited to `max_frame` (256 KiB), requests in flight to
`max_in_flight` (64), and a transfer keeps `pipeline_depth` requests (64) in
flight. Memory used by a read grows with the data actually received, never
with a size the server claims. A server that returns empty data without an end
of file, or more data than was asked for, ends the transfer with an error.

### Time

The client sets no time limits. Every operation can be bounded with
`tokio::time::timeout` and is cancel safe, but a request that receives no
reply keeps one of its in-flight slots until the connection ends. The server
limits the wait for the first packet (60 s by default) and can end idle
connections (`idle_timeout`, off by default). A client that stops reading
replies stalls its own connection only, for as long as the transport keeps
it open.

`LocalFs` runs at most 64 file system calls at once (`max_operations`), so a
stalled file system cannot take the whole of the program's blocking thread
pool. Set-user-id and set-group-id bits sent by a client are dropped unless
`special_mode_bits(true)` is set.

## `LocalFs`

`LocalFs` serves a directory of the local disk and keeps clients inside it.
What it guarantees and what it does not:

- **Escape by path.** `..` stops at the root, absolute paths start at the
  root, and symbolic links are resolved in the root's own namespace
  (`/etc/passwd` in a link is the served tree's `etc/passwd`). On Unix every
  step is made against an open directory descriptor with `O_NOFOLLOW`, so a
  component swapped for a link while a request runs makes the request fail
  instead of redirecting it. The tests include a thread that does exactly that
  as fast as it can.
- **Windows is narrower, and has a known gap.** Symbolic links, junctions
  and other reparse points are never followed or created, names with special
  meaning are refused (device names, `:`, backslashes, wildcards, trailing
  dots and spaces), the root is a verbatim path, and the directories above a
  target are held open without delete sharing while the request runs. The
  target itself cannot be held, so it is checked before the call and, for
  opens, on the handle. Hard-link and mount-point behaviour is that of the
  platform.

  **The gap:** a held directory can still be turned into a junction in place
  by another local process. If that happens between the check and the
  operation, the operation follows the junction out of the tree. Every
  operation checks the held directories again afterwards and fails if one
  changed; in testing this stopped data being read through the swap, but a
  file was still created outside, and a swap that is undone in time is not
  seen at all. Closing it needs operations relative to directory handles,
  which the standard library does not have. Clients cannot create junctions
  through this server, so the attack needs a second, local party with write
  access to the tree. **On Windows, do not serve a tree in which other local
  users can create junctions.** The ignored test
  `windows_junction_swap_by_a_local_process` demonstrates it. The Unix
  implementation does not have this gap.
- **Not covered:** another local process that can rename directories above
  the root while the server runs; hard links to outside files made by someone
  else; mount points inside the tree; absolute link targets that clients create
  are stored verbatim, so a program that reads the directory outside this
  server follows them to the host's files; `setstat` on a Unix file the server
  cannot open for reading can change times and ownership but not permissions.
  Special files other than regular files and directories are refused.
- **Information.** `readlink` returns link text unchanged, which can name a
  host path if an administrator made the link. Error messages are generic and
  carry no local paths.
- **Account names.** `users-groups-by-id@openssh.com` would tell clients the
  names of the host's users and groups. It is off unless
  `LocalFs::account_names(true)` is set.
- **Privileges.** The server does what its process may do. Run it as an
  unprivileged account, ideally one that owns nothing outside the root.

## Reading the server's own exposure

The `sftp_server` example is an example: one shared password, no rate limiting
beyond what ThirtySSH applies, no request log. Do not expose it as it is.
