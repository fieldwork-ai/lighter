#!/usr/bin/env bash
# The raw ceiling of a stream over our vsock device, in each direction.
#
#   scripts/vsock-ceiling.sh [bytes]     # default 4 GiB each way
#
# Boots a throwaway machine on the real CLI and talks to the agent's control
# channel — a unix socket on the Mac, the vsock proxy, the virtio-vsock
# device, the guest agent — asking it to `blast` that many bytes at us and
# then to `sink` that many from us. No TCP anywhere: this is the number the
# stream backend cannot beat without changing the device, and the one that
# decides whether phase 3 starts with the device or with the backend.
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
BYTES="${1:-4294967296}"
LIGHTER_BIN="${LIGHTER_BIN:-target/release/lighter}"
export LIGHTER_HOME="$(mktemp -d -t lighter-vsock)"
cleanup() {
	"$LIGHTER_BIN" stop >/dev/null 2>&1 || true
	[ -f "$LIGHTER_HOME/lighter.pid" ] && kill -9 "$(cat "$LIGHTER_HOME/lighter.pid")" 2>/dev/null || true
	rm -rf "$LIGHTER_HOME"
}
trap cleanup EXIT
"$LIGHTER_BIN" start >"$LIGHTER_HOME/start.log" 2>&1 &
for _ in $(seq 1 60); do [ -S "$LIGHTER_HOME/control.sock" ] && break; sleep 1; done
sleep 2
python3 - "$LIGHTER_HOME/control.sock" "$BYTES" <<'PY'
import socket, sys, time
path, n = sys.argv[1], int(sys.argv[2])
def gbits(nbytes, secs): return nbytes * 8 / secs / 1e9

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for opt in (socket.SO_SNDBUF, socket.SO_RCVBUF):
    s.setsockopt(socket.SOL_SOCKET, opt, 4 << 20)
s.connect(path)
s.sendall(b"ping\n"); assert s.recv(16).startswith(b"pong")
s.sendall(f"blast {n}\n".encode())
t = time.perf_counter(); left = n; buf = bytearray(1 << 20)
while left > 0:
    got = s.recv_into(buf, min(len(buf), left))
    if got == 0: sys.exit("guest closed early")
    left -= got
print("guest->host %.2f Gbit/s" % gbits(n, time.perf_counter() - t))

s.sendall(f"sink {n}\n".encode())
t = time.perf_counter(); chunk = b"\x5a" * (1 << 20); left = n
while left > 0:
    take = min(len(chunk), left); s.sendall(chunk[:take]); left -= take
reply = b""
while not reply.endswith(b"sunk\n"):
    r = s.recv(16)
    if not r: sys.exit("guest closed early")
    reply += r
print("host->guest %.2f Gbit/s" % gbits(n, time.perf_counter() - t))
PY
