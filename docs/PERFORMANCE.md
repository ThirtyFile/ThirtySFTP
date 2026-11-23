# Performance

How fast the client and server move files, how that was measured, and which
measurements the defaults come from. Numbers are from one machine and one
afternoon; use them for proportions, not as promises.

**Machine:** 32 hardware threads, Linux 6.18 under WSL2, OpenSSH 10.2p1
(Ubuntu 10.2p1-2ubuntu3.8), Rust release builds. Files live in `/dev/shm`, so
the disk does not decide anything. Everything runs on the loopback interface.

## Method

`crates/thirtysftp-bench` is a separate crate, so nothing it depends on reaches
the library.

- `thirtysftp-bench protocol` runs this crate's client against this crate's
  server over an in-memory pipe. The server stores nothing (reads give zeros,
  writes are dropped), so only the protocol is measured. `--latency-ms` delays
  everything on the pipe by that much in each direction.
- `crates/thirtysftp-bench/compare.sh` times whole transfers over SSH on
  loopback: this crate's client and OpenSSH's `sftp`, each against this crate's
  server and against OpenSSH's `sshd`. A run is one upload or download of a
  128 MiB random file; the median of the runs is reported. For `sftp` the time
  an empty session takes (connect, authenticate, exit) is measured separately
  and subtracted. `GRID=1` adds a sweep of request sizes and in-flight counts.

```sh
cargo run --release -p thirtysftp-bench -- protocol --size-mib 128
cargo run --release -p thirtysftp-bench -- protocol --size-mib 24 --runs 1 --latency-ms 20 \
    --chunks-kib 16,32,64,128,255 --depths 4,16,64,128
crates/thirtysftp-bench/compare.sh      # see the script for the environment it reads
```

## Results

### This crate against OpenSSH, over SSH on loopback

MiB/s, 128 MiB file, median of 5. Both clients use their defaults. Each client
negotiates its own cipher with each server, so a row compares clients on one
server, not servers.

| Client | Server | Upload | Download |
|---|---|---|---|
| thirtysftp | OpenSSH `sshd` | 439–485 | 471 |
| OpenSSH `sftp` | OpenSSH `sshd` | 416–443 | 446–479 |
| thirtysftp | thirtysftp server | 390–398 | 364–368 |
| OpenSSH `sftp` | thirtysftp server | 457–459 | 366–373 |

(The ranges are the spread over complete runs.) Against `sshd` the two clients
are within noise of each other. Against this crate's server, OpenSSH's `sftp`
uploads somewhat faster than this crate's client does; the download rates are
equal. Setting up the SSH session took about 430 ms against this crate's server
and about 80 ms against `sshd`; that is the SSH layer, not SFTP, and is
excluded from the rates above.

### The protocol alone

MiB/s, upload / download, 128 MiB, median of 3, no latency. The work per request
is the limit here, so small requests are slow, and from 16 requests in flight
upward the rate is flat.

| request KiB \ in flight | 1 | 4 | 16 | 64 | 128 |
|---|---|---|---|---|---|
| 8 | 82 / 80 | 295 / 303 | 1008 / 1201 | 1937 / 1933 | 2088 / 1896 |
| 16 | 164 / 152 | 558 / 626 | 1999 / 2150 | 3168 / 2415 | 3193 / 2483 |
| 32 | 297 / 264 | 956 / 1186 | 4129 / 2698 | 4076 / 2533 | 3277 / 2397 |
| 64 | 497 / 459 | 1682 / 2293 | 4661 / 2770 | 4318 / 2760 | 3486 / 2155 |
| 128 | 877 / 727 | 3443 / 2297 | 4267 / 2328 | 4182 / 2188 | 4009 / 1710 |
| 255 | 1400 / 1032 | 4555 / 2627 | 4664 / 2233 | 3805 / 2127 | 3238 / 1845 |

With 20 ms of delay each way (a 40 ms round trip) the rate is the data in
flight divided by the round trip, as it should be (24 MiB, one run):

| request KiB \ in flight | 4 | 16 | 64 | 128 |
|---|---|---|---|---|
| 16 | 1 / 1 | 6 / 6 | 23 / 23 | 47 / 46 |
| 32 | 3 / 3 | 12 / 12 | 47 / 47 | 91 / 91 |
| 64 | 6 / 6 | 24 / 23 | 93 / 91 | 176 / 174 |
| 128 | 12 / 12 | 47 / 46 | 182 / 176 | 264 / 272 |
| 255 | 23 / 22 | 81 / 80 | 271 / 251 | 444 / 486 |

### Request size and requests in flight, client against `sshd`

MiB/s, upload / download, median of 3, measured before the defaults changed;
every cell sets both values explicitly.

| request KiB \ in flight | 8 | 16 | 32 | 64 | 128 |
|---|---|---|---|---|---|
| 16 | 250 / 100 | 290 / 179 | 296 / 333 | 288 / 392 | 288 / 350 |
| 32 | 332 / 274 | 340 / 400 | 328 / 416 | 304 / 411 | 331 / 422 |
| 64 | 411 / 387 | 413 / 437 | 414 / 432 | 359 / 451 | 405 / 440 |
| 128 | 492 / 473 | 426 / 490 | 473 / 487 | 478 / 483 | 428 / 468 |
| 255 | 466 / 472 | 567 / 471 | 586 / 474 | 489 / 477 | 484 / 460 |

## What the defaults are, and why

- **Requests in flight per transfer: 64.** The tables show nothing gained
  above 16 on a fast link; on a slow one what matters is the product of this
  and the request size, and 64 is also what OpenSSH uses.
- **Request size: 32 KiB when the server states no limits, otherwise as much
  as the server allows, up to 128 KiB.** The 32 KiB figure is OpenSSH's, and a
  server that does not announce `limits@openssh.com` gives no better number.
  When it does, 128 KiB roughly matches OpenSSH's client (about 480 MiB/s
  against 300–340 at 32 KiB in the last table, an increase of about 45 %) and
  stays under the 255 KiB where nothing more is gained; at 255 KiB the memory
  held per transfer doubles. The change moved this client against `sshd` from
  334 / 430 to 485 / 471 MiB/s and against this crate's server from 255 / 311
  to 398 / 368.
- **Server read and write length: 128 KiB** (it was 64 KiB), for the same
  reason: it is what a client that follows `limits` will ask for. The bound on
  memory per connection is the number of requests being served (128) times
  this.

## Limits of these measurements

- Loopback has no latency and almost no loss. The 20 ms rows come from a delay
  on an in-memory pipe, not a network, and they leave out the SSH layer. Over
  SSH the channel window also bounds the data in flight. It is 2 MiB, the
  most ThirtySSH 0.2 allows for a channel read as a stream, so on a long link
  the SSH window, not the request size, limits a transfer to about one
  window per round trip (about 50 MiB/s at 40 ms).
- One machine, WSL2, a few runs of each configuration. Differences under about
  10 % are noise.
- Small files and many files are not measured; every number here is for one
  large file.
