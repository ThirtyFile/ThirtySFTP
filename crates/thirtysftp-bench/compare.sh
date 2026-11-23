#!/usr/bin/env bash
# Compares this crate's client with OpenSSH's `sftp`, each against this crate's
# server and against OpenSSH's `sshd`, over SSH on the loopback interface.
#
#   crates/thirtysftp-bench/compare.sh
#
# Environment (all optional):
#   SIZE_MIB      size of the transferred file, default 128
#   RUNS          repetitions, the median is reported, default 3
#   GRID=1        also time this client against `sshd` for a grid of request
#                 sizes and in-flight request counts
#   SSHD, SFTP_SERVER, SSHD_LIBEXEC
#                 where OpenSSH's `sshd` and `sftp-server` are, and the
#                 directory of `sshd`'s helper programs if it is not the
#                 usual one (see docs/INTEROP.md)
#   WORK_DIR      parent of the scratch directory; use a RAM disk such as
#                 /dev/shm (the default) so the disk does not decide the result
#
# Needs ssh-keygen and sftp on the PATH, python3 and a Rust toolchain.
set -euo pipefail

SIZE_MIB=${SIZE_MIB:-128}
RUNS=${RUNS:-3}
SSHD=${SSHD:-/usr/sbin/sshd}
SFTP_SERVER=${SFTP_SERVER:-/usr/lib/openssh/sftp-server}
WORK_DIR=${WORK_DIR:-/dev/shm}

cd "$(dirname "$0")/../.."
cargo build --release -p thirtysftp-bench
BENCH=${CARGO_TARGET_DIR:-target}/release/thirtysftp-bench

W=$(mktemp -d "$WORK_DIR/thirtysftp-bench.XXXXXX")
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$W"' EXIT
mkdir -p "$W/ours" "$W/theirs"
USERNAME=$(id -un)
ssh-keygen -q -t ed25519 -N '' -f "$W/client"
ssh-keygen -q -t ed25519 -N '' -f "$W/host"
head -c $((SIZE_MIB * 1024 * 1024)) /dev/urandom > "$W/local.bin"

# This crate's server.
"$BENCH" serve --root "$W/ours" --authorized-key "$W/client.pub" > "$W/serve.log" 2>&1 &
# OpenSSH's.
cp "$W/client.pub" "$W/authorized_keys"
chmod 600 "$W/authorized_keys"
SSHD_PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
{
  if [ -n "${SSHD_LIBEXEC:-}" ]; then
    echo "SshdSessionPath $SSHD_LIBEXEC/sshd-session"
    echo "SshdAuthPath $SSHD_LIBEXEC/sshd-auth"
  fi
  cat <<CONFIG
Port $SSHD_PORT
ListenAddress 127.0.0.1
HostKey $W/host
PidFile $W/sshd.pid
AuthorizedKeysFile $W/authorized_keys
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
StrictModes no
LogLevel ERROR
Subsystem sftp $SFTP_SERVER
CONFIG
} > "$W/sshd_config"
"$SSHD" -D -e -f "$W/sshd_config" > "$W/sshd.log" 2>&1 &
sleep 1.5
OURS_PORT=$(grep -o 'PORT [0-9]*' "$W/serve.log" | head -1 | cut -d' ' -f2)
echo "this crate's server on $OURS_PORT, sshd on $SSHD_PORT; $SIZE_MIB MiB, median of $RUNS; files in $WORK_DIR"
ssh -V 2>&1 | head -1

SFTP_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes -o PasswordAuthentication=no -o LogLevel=ERROR -i $W/client"

sftp_ms() { # port batch-file -> milliseconds one sftp run took
  local s e
  s=$(date +%s%N)
  sftp -P "$1" $SFTP_OPTS -b "$2" "$USERNAME@127.0.0.1" > /dev/null 2>&1 || echo "sftp failed" >&2
  e=$(date +%s%N)
  echo $(((e - s) / 1000000))
}
median() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }

bench_openssh() { # label port remote-dir
  printf 'bye\n' > "$W/b_base"
  printf 'put %s %s/bench.bin\n' "$W/local.bin" "$3" > "$W/b_put"
  printf 'get %s/bench.bin %s/back.bin\n' "$3" "$W" > "$W/b_get"
  local base ups downs
  base=$(for _ in 1 2 3; do sftp_ms "$2" "$W/b_base"; done | median)
  ups=$(for _ in $(seq "$RUNS"); do sftp_ms "$2" "$W/b_put"; done | median)
  downs=$(for _ in $(seq "$RUNS"); do sftp_ms "$2" "$W/b_get"; done | median)
  cmp "$W/local.bin" "$W/back.bin" || echo "DATA DIFFERS"
  rm -f "$W/back.bin"
  # The time an empty session takes (connect, authenticate, exit) is removed.
  awk -v l="$1" -v m="$SIZE_MIB" -v b="$base" -v u="$ups" -v d="$downs" 'BEGIN {
    printf "OpenSSH sftp -> %-13s upload %5.0f MiB/s, download %5.0f MiB/s (%d ms session overhead removed)\n",
      l, m / ((u - b) / 1000), m / ((d - b) / 1000), b }'
}

bench_ours() { # label port remote-dir [extra args]
  local label=$1 port=$2 dir=$3
  shift 3
  "$BENCH" client --port "$port" --user "$USERNAME" --key "$W/client" --dir "$dir" \
    --size-mib "$SIZE_MIB" --runs "$RUNS" "$@" |
    sed "s#^thirtysftp over ThirtySSH#thirtysftp    -> $label#"
}

bench_ours "this server" "$OURS_PORT" ""
bench_openssh "this server" "$OURS_PORT" ""
bench_ours "OpenSSH sshd" "$SSHD_PORT" "$W/theirs"
bench_openssh "OpenSSH sshd" "$SSHD_PORT" "$W/theirs"

if [ "${GRID:-0}" = 1 ]; then
  echo
  echo "thirtysftp -> OpenSSH sshd, by request size and requests in flight:"
  for chunk in 16 32 64 128 255; do
    for depth in 8 16 32 64 128; do
      printf 'chunk %3s KiB, in flight %3s: ' "$chunk" "$depth"
      "$BENCH" client --port "$SSHD_PORT" --user "$USERNAME" --key "$W/client" --dir "$W/theirs" \
        --size-mib "$SIZE_MIB" --runs "$RUNS" --chunk-kib "$chunk" --depth "$depth" |
        sed 's#.*: upload#upload#'
    done
  done
fi
