#!/usr/bin/env bash
# Milestone 20 gate: no peer can silence the machine (#84).
#
# Everything the guest sends the Mac shares one transmit ring of 256 chains,
# and a chain holding a payload is held until that connection's host peer
# takes the bytes. On 0.13.4 one published port written in small pieces to a
# client that stopped reading held every chain, and Docker, every other port
# and the memory reports went dark until a restart. What has to hold now,
# whatever a peer does:
#
#   1. a client that stops reading, then resets, at every write size from
#      4 KiB to 256 KiB: Docker and an unrelated port answer throughout, the
#      stream holds no more than its share of the ring, and every chain is
#      back once it resets
#   2. the same through a TLS reverse proxy, which writes 16 KiB records
#   3. a client that half-closes and reads to the end gets every byte
#   4. a client reading slowly is slowed, never reset, and gets every byte
#   5. hundreds of clients that stop reading, then reset (as many as half
#      the Mac's network buffers and half the guest hold): the same, and the
#      table empties
#   6. a guest whose memory is in pieces (no free block of 256 KiB): bytes
#      into the machine keep their speed and their receive buffers come
#      from the pool, and (1) holds there too
#   7. a client that closes while its server says nothing is let go, over a
#      published port (TCP, found by keepalive) and over the Docker socket,
#      and one that only half-closed still gets a reply that comes late (#87)
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

PROFILE="${PROFILE:-release}"
LIGHTER="${LIGHTER_BIN:-target/$PROFILE/lighter}"
FAILED=0
pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
note() { printf '  \033[33m··\033[0m   %s\n' "$*"; }

ulimit -n 32768 || { echo "m20 needs a 32768-file limit: up to 2,000 clients are held at once" >&2; exit 1; }

export LIGHTER_HOME="$(mktemp -d -t lighter-m20-home)"
export DOCKER_HOST="unix://$LIGHTER_HOME/docker.sock"
SCRATCH="$HOME/.lighter-gate-m20-$$"
mkdir -p "$SCRATCH"
cleanup() {
	docker rm -f m20-push m20-ok m20-tls m20-sink m20-frag m20-silent >/dev/null 2>&1 || true
	docker network rm m20-net >/dev/null 2>&1 || true
	"$LIGHTER" stop >/dev/null 2>&1 || true
	[ "$FAILED" = 0 ] || { mkdir -p "$ROOT/.logs"; cp "$LIGHTER_HOME/machine.log" "$ROOT/.logs/m20-machine-$(date +%H%M%S).log" 2>/dev/null; } || true
	rm -rf "$LIGHTER_HOME" "$SCRATCH"
}
trap cleanup EXIT

PYTHON="python:3.12-slim"
NGINX="nginx:1.27-alpine"
PUSH=18096
OK=18097
TLS=18443
SINK=18098
SILENT=18099

# Seconds a command takes, or "timeout" past its limit.
timed() {
	python3 -c 'import subprocess, sys, time
t = time.monotonic()
try:
    r = subprocess.run(sys.argv[2:], timeout=float(sys.argv[1]), capture_output=True)
    why = (r.stderr.decode(errors="replace").strip().splitlines() or [""])[-1][:160]
    print("%.2f" % (time.monotonic() - t) if r.returncode == 0 else "exit%d (%s)" % (r.returncode, why))
except subprocess.TimeoutExpired:
    print("timeout")' "$@"
}
fast() { awk -v t="$1" -v l="$2" 'BEGIN { exit !(t != "timeout" && t !~ /^exit/ && t + 0 < l) }'; }

# The vsock state, from the device's own dump: the ring's summary line, and
# the most chains any connection of the push port holds.
dump() {
	kill -INFO "$(cat "$LIGHTER_HOME/lighter.pid")"
	sleep 2
	sed 's/\x1b\[[0-9;]*m//g' "$LIGHTER_HOME/machine.log" | awk '/DUMP begin/ { block = "" } /DUMP vsock/ { block = block $0 "\n" } END { printf "%s", block }' >"$SCRATCH/dump"
}
ring_held() { grep -o 'queues held=[0-9]*' "$SCRATCH/dump" | head -1 | cut -d= -f2; }
port_conns() { grep -c "a client of port $1 " "$SCRATCH/dump"; }
port_held() { grep "a client of port $1 " "$SCRATCH/dump" | grep -o ' held=[0-9]*' | cut -d= -f2 | sort -n | tail -1; }

# Docker and an unrelated published port, both inside two seconds.
alive() {
	local what="$1" d o
	d="$(timed 10 docker ps -q)"
	o="$(timed 10 /usr/bin/curl -s -m 9 "http://127.0.0.1:$OK/")"
	if fast "$d" 2 && fast "$o" 2; then
		pass "$what: Docker answered in ${d} s and an unrelated port in ${o} s"
	else
		fail "$what: Docker ${d}, an unrelated port ${o}"
	fi
}

echo "==> Booting"
"$LIGHTER" config --cpus 4 --memory 4096 >/dev/null 2>&1
"$LIGHTER" start >/dev/null 2>&1 || { fail "the machine did not start"; exit 1; }
docker pull -q "$PYTHON" >/dev/null && docker pull -q "$NGINX" >/dev/null || { fail "could not pull the images"; exit 1; }
docker network create m20-net >/dev/null

# The gate's own servers are never the OOM killer's choice: fragmenting the
# guest takes it to the edge of its memory, and twice the killer took the
# push server, failing a check that is about the link.
# The server: `GET /<piece>[/<total>]` answers HTTP and then bytes in pieces
# of that size, forever or to the total, each byte its offset modulo 251 so
# a client can check what it got. A short sleep between pieces keeps each
# piece one guest packet.
cat >"$SCRATCH/push.py" <<'EOF'
import socket, threading, time
# Bounded per client, so a flood of them tests the link rather than the
# guest's memory: one thread each at 8 MiB stacks and a send buffer that
# autotunes to 4 MiB ran a 4 GiB guest out of memory at two thousand.
threading.stack_size(256 * 1024)
def serve(c):
    try:
        line = c.recv(4096).split(b"\r\n")[0].decode()
        parts = line.split()[1].strip("/").split("/")
        piece = int(parts[0]); total = int(parts[1]) if len(parts) > 1 else None
        c.sendall(b"HTTP/1.0 200 OK\r\n\r\n")
        block = bytes(i % 251 for i in range(251 * (1 + 262144 // 251)))
        sent = 0
        while total is None or sent < total:
            n = piece if total is None else min(piece, total - sent)
            at = sent % 251
            c.sendall(block[at:at + n])
            sent += n
            if piece < 65536:
                time.sleep(0.0005)
    except Exception:
        pass
    finally:
        c.close()
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("0.0.0.0", 8096)); s.listen(4096)
while True:
    c, _ = s.accept()
    c.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 65536)
    threading.Thread(target=serve, args=(c,), daemon=True).start()
EOF
docker run -d --name m20-push --oom-score-adj=-1000 --network m20-net -p "$PUSH:8096" -v "$SCRATCH:/g:ro" "$PYTHON" python3 /g/push.py >/dev/null
docker run -d --name m20-ok --oom-score-adj=-1000 -p "$OK:8000" "$PYTHON" python3 -m http.server 8000 >/dev/null
# A server that says nothing: it reads a request to its end, then for
# `hold` keeps the connection open and silent for good, and for `late`
# answers 20 s later.
cat >"$SCRATCH/silent.py" <<'EOF'
import socket, threading, time
def serve(c):
    request = b""
    while True:
        data = c.recv(4096)
        if not data:
            break
        request += data
    if request.startswith(b"late"):
        time.sleep(20)
        c.sendall(b"late")
        c.close()
        return
    time.sleep(10 ** 6)
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("0.0.0.0", 8099)); s.listen(64)
while True:
    c, _ = s.accept()
    threading.Thread(target=serve, args=(c,), daemon=True).start()
EOF
docker run -d --name m20-silent --oom-score-adj=-1000 -p "$SILENT:8099" -v "$SCRATCH:/g:ro" "$PYTHON" python3 /g/silent.py >/dev/null
/usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -subj /CN=m20 -days 1 -keyout "$SCRATCH/key.pem" -out "$SCRATCH/cert.pem" >/dev/null 2>&1
cat >"$SCRATCH/nginx.conf" <<'EOF'
events {}
http {
    server {
        listen 443 ssl;
        ssl_certificate /g/cert.pem;
        ssl_certificate_key /g/key.pem;
        location / { proxy_pass http://m20-push:8096; proxy_buffering off; }
    }
}
EOF
docker run -d --name m20-tls --oom-score-adj=-1000 --network m20-net -p "$TLS:443" -v "$SCRATCH:/g:ro" "$NGINX" nginx -c /g/nginx.conf -g 'daemon off;' >/dev/null
for i in $(seq 1 30); do /usr/bin/curl -s -m 1 "http://127.0.0.1:$OK/" >/dev/null && break; sleep 1; done
# And the push server itself, serving a known stream end to end.
served=""
for i in $(seq 1 30); do
	served="$(/usr/bin/curl -s -m 2 "http://127.0.0.1:$PUSH/1024/1024" | wc -c | tr -d ' ')"
	[ "$served" = 1024 ] && break
	sleep 1
done
[ "$served" = 1024 ] || { fail "the push server did not serve: $(docker logs m20-push 2>&1 | tail -2)"; exit 1; }

# A client on the Mac: connects to a port, asks for a stream, reads `rate`
# bytes a second (0: nothing at all) for `hold` seconds, then resets the
# connection or, with `half`, reads to the end after closing its own side.
cat >"$SCRATCH/client.py" <<'EOF'
import socket, ssl, struct, sys, time, hashlib
port, path, hold, rate = int(sys.argv[1]), sys.argv[2], float(sys.argv[3]), int(sys.argv[4])
mode = sys.argv[5] if len(sys.argv) > 5 else "reset"
raw = socket.socket(); raw.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
raw.connect(("127.0.0.1", port))
c = raw
if mode == "tls":
    ctx = ssl.create_default_context(); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
    c = ctx.wrap_socket(raw)
c.sendall(("GET /%s HTTP/1.0\r\n\r\n" % path).encode())
if mode == "half":
    c.shutdown(socket.SHUT_WR)
got = b""; deadline = time.time() + hold
while time.time() < deadline and rate:
    data = c.recv(min(rate, 65536))
    if not data:
        break
    got += data
    time.sleep(min(rate, 65536) / rate)
if mode in ("half", "slow"):
    while True:
        data = c.recv(65536)
        if not data:
            break
        got += data
    body = got.split(b"\r\n\r\n", 1)[1]
    ok = all(b == i % 251 for i, b in enumerate(body))
    print(len(body), "intact" if ok else "corrupt")
else:
    time.sleep(max(0, deadline - time.time()))
    c.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
    c.close()
EOF

# M20_ONLY=frag runs the last case alone, for working on it.
if [ "${M20_ONLY:-}" != frag ]; then
echo "==> A client that stops reading, then resets"
for piece in 4096 8192 16384 65536 262144; do
	python3 "$SCRATCH/client.py" "$PUSH" "$piece" 10 0 &
	client=$!
	sleep 7
	dump
	held="$(port_held $PUSH)"; ring="$(ring_held)"
	alive "$((piece / 1024)) KiB pieces, the client not reading"
	[ "${held:-999}" -le 64 ] && [ "${ring:-999}" -le 192 ] \
		&& pass "$((piece / 1024)) KiB pieces: the stream holds ${held} chains of its share of 64, the ring ${ring} of its limit of 192" \
		|| fail "$((piece / 1024)) KiB pieces: the stream holds ${held:-?} chains, the ring ${ring:-?}"
	wait "$client"
	sleep 3
	dump
	[ "$(port_conns $PUSH)" = 0 ] && [ "$(ring_held)" = 0 ] \
		&& pass "$((piece / 1024)) KiB pieces: reset, and every chain is back" \
		|| fail "$((piece / 1024)) KiB pieces: after the reset $(port_conns $PUSH) connections remain and the ring holds $(ring_held)"
	alive "$((piece / 1024)) KiB pieces, after the reset"
done

echo "==> The same through a TLS reverse proxy (16 KiB records)"
python3 "$SCRATCH/client.py" "$TLS" 65536 10 0 tls &
client=$!
sleep 7
dump
held="$(port_held $TLS)"
alive "TLS, the client not reading"
[ "${held:-999}" -le 64 ] && pass "TLS: the stream holds ${held} chains" || fail "TLS: the stream holds ${held:-?} chains"
wait "$client"
sleep 3
dump
[ "$(port_conns $TLS)" = 0 ] && pass "TLS: reset, and every chain is back" || fail "TLS: $(port_conns $TLS) connections remain"

echo "==> A client that half-closes, and one that reads slowly"
got="$(python3 "$SCRATCH/client.py" "$PUSH" 16384/52428800 0 0 half)"
[ "$got" = "52428800 intact" ] && pass "half-close: 50 MiB after the request side closed, every byte intact" || fail "half-close: got '$got'"
before="$("$LIGHTER" doctor 2>/dev/null | grep "vsock link")"
got="$(python3 "$SCRATCH/client.py" "$PUSH" 16384/4194304 20 204800 slow)"
after="$("$LIGHTER" doctor 2>/dev/null | grep "vsock link")"
[ "$got" = "4194304 intact" ] && pass "slow reader: 4 MiB at 200 KB/s, every byte intact" || fail "slow reader: got '$got'"
grep -q "no resets" <<<"$after" && pass "slow reader: slowed, never reset" || fail "slow reader: $after"

echo "==> A client that closes while its server says nothing"
# Over a published port the client is TCP, which looks the same half-closed
# and closed once its FIN is read: keepalive tells, once the client's
# kernel lets go of the connection (FIN_WAIT_2, 60 s on macOS).
python3 -c 'import socket, sys
c = socket.create_connection(("127.0.0.1", int(sys.argv[1]))); c.sendall(b"hold"); c.close()' "$SILENT"
sleep 2
dump
[ "$(port_conns $SILENT)" = 1 ] || note "published port: the silent connection was not in the table to begin with ($(port_conns $SILENT))"
gone=""
for t in $(seq 10 10 120); do
	sleep 8
	dump
	[ "$(port_conns $SILENT)" = 0 ] && { gone=$t; break; }
done
[ -n "$gone" ] && pass "published port: a client that closed is let go within ${gone} s, its server silent" \
	|| fail "published port: a client that closed is still held after 120 s, its server silent"
# A half-closed one is still there, and gets the reply that comes late.
got="$(timed 45 python3 -c 'import socket, sys
c = socket.create_connection(("127.0.0.1", int(sys.argv[1]))); c.sendall(b"late"); c.shutdown(socket.SHUT_WR)
got = b""
while True:
    d = c.recv(64)
    if not d:
        break
    got += d
assert got == b"late", got' "$SILENT")"
fast "$got" 45 && pass "published port: a client that half-closed still got a reply 20 s late" \
	|| fail "published port: a client that half-closed lost its late reply ($got)"
# Over the Docker socket, a Unix socket: a client killed while its exec's
# process says nothing held its connection until the process exited.
dump
before="$(grep -c "a client of the Docker socket" "$SCRATCH/dump")"
docker exec -i m20-silent sleep 600 </dev/null &
exec_cli=$!
sleep 3
dump
during="$(grep -c "a client of the Docker socket" "$SCRATCH/dump")"
kill -9 "$exec_cli" 2>/dev/null
wait "$exec_cli" 2>/dev/null
sleep 12
dump
after="$(grep -c "a client of the Docker socket" "$SCRATCH/dump")"
[ "$during" -gt "$before" ] && [ "$after" -le "$before" ] \
	&& pass "Docker socket: a client killed during a silent exec is let go ($before, $during while it ran, $after 12 s after)" \
	|| fail "Docker socket: connections $before before, $during during, $after after a client killed during a silent exec"

# As many clients as half the Mac's own network buffers hold at a default
# 128 KiB socket buffer each, up to two thousand: an 8 GB Mac has 128 MiB
# of clusters, and two thousand idle clients outran it whatever lighter did.
MANY=$(( $(sysctl -n kern.ipc.nmbclusters) * 2048 / 2 / 131072 ))
[ "$MANY" -gt 2000 ] && MANY=2000
# And as many as half the guest holds at 2 MiB each: what a client that
# stops reading costs it, about 1 MiB of TCP's queues, a quarter of the
# vsock backlog's least (guest patch 0056) and the push server's thread for
# it. Before 0056 it was 5.6 MiB, and two thousand ran a 4 GiB guest out of
# memory.
GUEST_MANY=$(( 4096 / 2 / 2 ))
[ "$MANY" -gt "$GUEST_MANY" ] && MANY=$GUEST_MANY
# What the guest has available, from `lighter status`, in MiB.
available() { "$LIGHTER" status 2>/dev/null | sed -n 's/.*guest mem *\([0-9]*\) of.*/\1/p'; }
echo "==> $MANY clients that stop reading, then reset"
avail_before="$(available)"
cat >"$SCRATCH/many.py" <<'EOF'
import socket, struct, sys, time
port, n, hold = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3])
socks = []
for i in range(n):
    s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
    s.connect(("127.0.0.1", port)); s.sendall(b"GET /16384 HTTP/1.0\r\n\r\n"); socks.append(s)
time.sleep(hold)
for s in socks:
    s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0)); s.close()
print(len(socks))
EOF
python3 "$SCRATCH/many.py" "$PUSH" "$MANY" 20 >"$SCRATCH/many.out" &
many=$!
sleep 16
dump
avail_during="$(available)"
if [ -n "$avail_before" ] && [ -n "$avail_during" ]; then
	each=$(( (avail_before - avail_during) * 1024 / MANY ))
	[ "$each" -le 3072 ] \
		&& pass "$MANY clients: each costs the guest ${each} KiB (${avail_before} MiB available before, ${avail_during} with them)" \
		|| fail "$MANY clients: each costs the guest ${each} KiB, more than 3 MiB (${avail_before} MiB available before, ${avail_during} with them)"
else
	fail "$MANY clients: lighter status gave no guest memory ('$avail_before', '$avail_during')"
fi
ring="$(ring_held)"
alive "$MANY clients not reading"
[ "${ring:-999}" -le 192 ] && pass "$MANY clients: the ring holds ${ring} chains of its limit of 192" || fail "$MANY clients: the ring holds ${ring:-?}"
# And the Mac's own network buffers: lighter leaves TCP sockets to macOS's
# autotuning, so clients that do not read do not hold them (sockbuf.rs).
# What has to hold is the two answers above; a Mac whose buffers ran out
# regardless (an 8 GB one has a quarter of a Studio's) is said, and waited
# out (reactor.rs, `starve`).
nobufs="$(sed 's/\x1b\[[0-9;]*m//g' "$LIGHTER_HOME/machine.log" | grep -c "out of network buffers")"
[ "$nobufs" = 0 ] && pass "$MANY clients: the Mac never ran out of network buffers" || note "$MANY clients: the Mac ran short of network buffers (ENOBUFS), and streams waited for them"
wait "$many"
sleep 12
dump
[ "$(port_conns $PUSH)" = 0 ] && [ "$(ring_held)" = 0 ] \
	&& pass "$MANY clients: reset, the table is empty and every chain is back" \
	|| fail "$MANY clients: $(port_conns $PUSH) connections remain, the ring holds $(ring_held)"

fi

echo "==> A guest whose memory is in pieces"
# Bytes into the machine: the Mac writes as fast as it can to a container
# that discards them.
docker run -d --name m20-sink --oom-score-adj=-1000 -p "$SINK:8000" "$PYTHON" python3 -c '
import socket, threading
def sink(c):
    while c.recv(1 << 20): pass
    c.close()
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("0.0.0.0", 8000)); s.listen(64)
while True:
    c, _ = s.accept(); threading.Thread(target=sink, args=(c,), daemon=True).start()' >/dev/null
sleep 2
inward() {
	python3 -c '
import socket, sys, time
c = socket.create_connection(("127.0.0.1", int(sys.argv[1]))); buf = bytes(1 << 20); n = 0; t = time.monotonic()
while time.monotonic() - t < 8:
    c.sendall(buf); n += len(buf)
c.close(); print("%.1f" % (n * 8 / (time.monotonic() - t) / 1e9))' "$SINK"
}
# The receive pool's counters (guest patch 0054), as `key=value` words.
pool() {
	docker run --rm --privileged "$PYTHON" cat /sys/module/vmw_vsock_virtio_transport/parameters/rx_pool 2>/dev/null
}
counter() { tr ' ' '\n' <<<"$1" | sed -n "s/^$2=//p"; }
base="$(inward)"
# Fragmented as a media server's library scan leaves a guest: inodes and
# dentries pinning scattered slab pages, and the rest of free memory pinned
# a page per block (fixtures/fragment.py). Memory available, in pieces.
docker run -d --name m20-frag --privileged -v "$ROOT/scripts/gates/fixtures:/f:ro" "$PYTHON" python3 /f/fragment.py >/dev/null
for i in $(seq 1 120); do docker logs m20-frag 2>&1 | grep -q fragmented && break; sleep 2; done
read -r _ free256 slab <<<"$(docker logs m20-frag 2>&1 | grep fragmented | tail -1)"
memory="$("$LIGHTER" status 2>/dev/null | grep "guest mem" | sed 's/^ *guest mem *//')"
# Fewer free blocks than one ring of receive buffers: refills cannot come
# from the allocator, which is what the pool is for.
[ "${free256:-999}" -lt 128 ] \
	&& pass "fragmented: ${free256} free blocks of 256 KiB with ${slab} MiB of pinned metadata; ${memory}" \
	|| fail "the guest was not fragmented: ${free256:-?} free blocks of 256 KiB ($(docker logs m20-frag 2>&1 | tail -1))"
p0="$(pool)"
[ -n "$p0" ] || { fail "this guest has no receive buffer pool (guest patch 0054)"; p0="pool=0 fast=0 slow=0 small=0"; }
frag="$(inward)"
p1="$(pool)"
[ -n "$p1" ] || p1="$p0"
fast=$(( $(counter "$p1" fast) - $(counter "$p0" fast) ))
slow=$(( $(counter "$p1" slow) - $(counter "$p0" slow) ))
small=$(( $(counter "$p1" small) - $(counter "$p0" small) ))
awk -v f="$frag" -v b="$base" 'BEGIN { exit !(f + 0 >= 0.8 * b) }' \
	&& pass "fragmented: bytes into the machine at ${frag} Gbit/s against ${base} unfragmented" \
	|| fail "fragmented: bytes into the machine at ${frag} Gbit/s against ${base} unfragmented"
# Of every buffer posted, at least 95 in 100 recycled, and at most one in a
# thousand small: one is posted only while every pooled buffer is in use.
[ "$fast" -gt 0 ] && [ "$((slow * 20))" -le "$((fast + slow))" ] && [ "$((small * 1000))" -le "$((fast + slow))" ] \
	&& pass "fragmented: ${fast} receive buffers recycled by the pool, ${slow} newly allocated, ${small} small" \
	|| fail "fragmented: the pool recycled ${fast}, allocated ${slow} anew and fell back to small ${small} times ($p1)"
python3 "$SCRATCH/client.py" "$PUSH" 16384 10 0 &
client=$!
sleep 7
dump
held="$(port_held $PUSH)"
alive "fragmented, a client not reading"
[ "${held:-999}" -le 64 ] && pass "fragmented: the stream holds ${held} chains" || fail "fragmented: the stream holds ${held:-?} chains"
wait "$client"
docker rm -f m20-frag >/dev/null

echo
[ "$FAILED" -eq 0 ] && echo "m20-hostile: all checks passed" || echo "m20-hostile: FAILED"
exit "$FAILED"
