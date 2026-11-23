# Interoperability with OpenSSH

The tests in `crates/thirtysftp/tests/openssh.rs` check this crate against
OpenSSH programs. They are `#[ignore]`d because they need those programs, and
run in CI on Linux.

## What is covered

| Direction | OpenSSH program | Tests |
|---|---|---|
| This crate's client, over a pipe | `sftp-server` | handshake and announced extensions, 6 MiB transfers with the default chunk size and with one larger than the server answers (short reads), ranged reads, directories and error codes, symbolic links (OpenSSH's argument order), attributes, every extension the server offers, open modes, offsets past 4 GiB, a 3000-entry directory, 16 transfers at once |
| This crate's client, over ThirtySSH | `sshd` (running `sftp-server` as its subsystem) | key authentication, transfers, two SFTP sessions on one connection, rekeying during a transfer |
| OpenSSH's `sftp` client | this crate's server | `pwd`, `ls -l`, `get`, `put`, `put -r`, `get -r`, `mkdir`, `rename`, `ln -s`, `chmod`, `df`, `rm`, `rmdir`, `reget`, `reput`, `version`; attempts to leave the served directory |
| OpenSSH's `scp` (SFTP mode, `-s`) | this crate's server | upload and download of a 2 MiB file |

## Running them

```sh
cargo test -p thirtysftp --test openssh -- --ignored
```

Needs `ssh-keygen`, `sftp` and `scp` on the `PATH`, and the `sftp-server` and
`sshd` programs, which are searched for in the usual places and can be named
with the environment variables `SFTP_SERVER` and `SSHD`. The `sshd` test runs
the daemon as the current user on `127.0.0.1`, which needs its privilege
separation directory to exist (`sudo mkdir -p /run/sshd` on Debian and Ubuntu).

## Versions

Recorded as they are run. CI prints the installed versions at the start of
the `OpenSSH interoperability` job.

| OpenSSH | Where | OpenSSH `sftp` and `scp` against this server | This client against `sftp-server` and `sshd` |
|---|---|---|---|
| 10.2p1 (Ubuntu 10.2p1-2ubuntu3.8) | local, Linux | passed (4 tests) | passed (10 tests) |

The local run used the `sftp-server` and `sshd` programs unpacked from the
distribution's packages into a scratch directory, started with `SFTP_SERVER`,
`SSHD` and `SSHD_LIBEXEC` pointing there.

## Findings from interoperability testing

- OpenSSH's `sftp` only uses `statvfs@openssh.com` (its `df` command) if the
  server announces it as version `2`. The server does.
- `SYMLINK` carries the target first and the link second, the reverse of the
  protocol draft. The client and the server follow OpenSSH.
