#!/usr/bin/env bash
# Milestone 3 gate, part four: TCP as streams, the semantics.
#
# The stream path replaces a network stack with a byte copy, and the ways
# that can go subtly wrong are all about connection lifecycle rather than
# throughput. Each check here is one of them:
#
#   tls        — a real TLS handshake and page over the stream
#   refused    — a closed port on the Mac is refused promptly, not hung
#   halfclose  — a client that sends and half-closes still gets its reply
#   host       — host.docker.internal reaches a server on the Mac
#   published  — a published port answers, and closes when the container stops
#   many       — a thousand concurrent connections from one container
#   dns, icmp  — what stays on the network device still works
#   ipv6       — a v6 destination, when the Mac has one
set -u
# The connection fixture keeps a thousand sockets open. A shell inherited
# from a GUI app may have a soft limit of only 256.
ulimit -n 32768 || { echo "the stream gate needs a 32768-file limit: 10,000 inbound connections are held at once" >&2; exit 1; }
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
LIGHTER="${LIGHTER_BIN:-target/release/lighter}"
export LIGHTER_STREAMS=1
export LIGHTER_HOME="$(mktemp -d -t lighter-m3s)"
if [ -n "${LIGHTER_BENCH_OWNER_FILE:-}" ]; then
	python3 - "$LIGHTER_BENCH_OWNER_FILE" "$LIGHTER_HOME/lighter.app/Contents/MacOS/lighter" <<'PYOWNER'
import json, sys
with open(sys.argv[1], 'a') as f:
    f.write(json.dumps(sys.argv[2]) + '\n')
PYOWNER
fi
D="docker -H unix://$LIGHTER_HOME/docker.sock"
FAILED=0
pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
HTTP_PID=""
HOLD_PID=""
FLOOD_HOLD_PID=""
FLOOD_IN_PID=""
cleanup() {
	mkdir -p .logs
	[ ! -f "$LIGHTER_HOME/machine.log" ] || cp "$LIGHTER_HOME/machine.log" .logs/m3-streams-last-boot.log
	[ ! -f "$LIGHTER_HOME/http.log" ] || cp "$LIGHTER_HOME/http.log" .logs/m3-streams-last-http.log
	[ -z "$HTTP_PID" ] || kill "$HTTP_PID" 2>/dev/null
	[ -z "$HOLD_PID" ] || kill "$HOLD_PID" 2>/dev/null
	[ -z "$FLOOD_HOLD_PID" ] || kill "$FLOOD_HOLD_PID" 2>/dev/null
	[ -z "$FLOOD_IN_PID" ] || kill "$FLOOD_IN_PID" 2>/dev/null
	"$LIGHTER" stop >/dev/null 2>&1 || true
	[ -f "$LIGHTER_HOME/lighter.pid" ] && kill -9 "$(cat "$LIGHTER_HOME/lighter.pid")" 2>/dev/null || true
	rm -rf "$LIGHTER_HOME"
}
trap cleanup EXIT

echo "==> Booting with streams"
"$LIGHTER" start >"$LIGHTER_HOME/start.log" 2>&1 &
for _ in $(seq 1 60); do $D info >/dev/null 2>&1 && break; sleep 1; done
$D info >/dev/null 2>&1 || { fail "machine did not come up"; exit 1; }
grep -q "INIT streams=on" "$LIGHTER_HOME/machine.log" && pass "the guest installed its redirect" || fail "guest: $(grep -o 'INIT streams=.*' "$LIGHTER_HOME/machine.log" || echo 'no streams line')"
if python3 - <<'PY'
import os, socket, sys
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as control:
    control.settimeout(15)
    control.connect(os.path.join(os.environ['LIGHTER_HOME'], 'control.sock'))
    control.sendall(b'sh /sbin/lighter-agent --bpf-rollback-test\n')
    response = b''
    while not response.endswith(b'--end--\n'):
        chunk = control.recv(65536)
        if not chunk:
            break
        response += chunk
    print(response.decode(errors='replace'))
    sys.exit(0 if b'PASS:' in response and response.endswith(b'exit=0\n--end--\n') else 1)
PY
then pass "failed sockmap joins clean up and preserve fallback I/O"
else fail "sockmap rollback regression"
fi
$D pull -q curlimages/curl:8.11.1 >/dev/null 2>&1
$D pull -q alpine:3.21 >/dev/null 2>&1
LAN_IP="$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1)"

# tls
code="$($D run --rm curlimages/curl:8.11.1 -s -o /dev/null -w '%{http_code}' --max-time 15 https://example.com 2>/dev/null)"
[ "$code" = 200 ] && pass "TLS to example.com over the stream" || fail "TLS: http_code=${code:-none}"

# refused: nothing listens on 9 on the Mac
t0=$(date +%s); out="$($D run --rm curlimages/curl:8.11.1 -s -o /dev/null -w '%{exitcode}' --max-time 10 "http://$LAN_IP:9/" 2>/dev/null)"; dt=$(( $(date +%s) - t0 ))
{ [ "$out" != 0 ] && [ "$dt" -le 3 ]; } && pass "a closed port is refused in ${dt}s (curl exit $out)" || fail "refused: exit=$out after ${dt}s"

# halfclose: a client that sends its request and closes its write side
# must still get the reply. Against a server on the Mac that answers after
# EOF (busybox nc cannot half-close and example.com drops a connection that
# does, so neither is a test of the path).
python3 - <<'PY' &
import socket
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); srv.bind(("0.0.0.0", 18096)); srv.listen(5)
while True:
    c, _ = srv.accept()
    while c.recv(4096): pass
    c.sendall(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok"); c.close()
PY
HALF_PID=$!
sleep 1
$D pull -q node:24-alpine >/dev/null 2>&1
reply="$($D run --rm node:24-alpine node -e '
const net = require("net");
const s = net.connect(18096, "'"$LAN_IP"'", () => { s.write("GET / HTTP/1.0\r\n\r\n"); s.end(); });
let got = ""; s.on("data", d => { got += d; }); s.on("close", () => { console.log(got.split("\r\n")[0]); process.exit(0); });
s.on("error", e => { console.log("error " + e.message); process.exit(1); }); setTimeout(() => { console.log("timeout"); process.exit(1); }, 15000);
' 2>/dev/null)"
kill "$HALF_PID" 2>/dev/null
echo "$reply" | grep -q "HTTP/1.0 200" && pass "half-close: the reply arrives after the request side closed" || fail "half-close: got '${reply}'"

# host.docker.internal -> a server on the Mac
# http.server reverse-resolves its bind address before listening. That
# blocked for tens of seconds on the M1; this fixture needs no host lookup.
python3 -u - >"$LIGHTER_HOME/http.log" 2>&1 <<'PY' &
import socket
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 18099))
srv.listen(5)
print("HTTP fixture ready", flush=True)
while True:
    c, _ = srv.accept()
    with c:
        c.settimeout(5)
        try:
            c.recv(4096)
            c.sendall(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok")
        except OSError:
            pass
PY
HTTP_PID=$!
for _ in $(seq 1 100); do
	grep -q 'HTTP fixture ready' "$LIGHTER_HOME/http.log" && break
	kill -0 "$HTTP_PID" 2>/dev/null || break
	sleep 0.1
done
if grep -q 'HTTP fixture ready' "$LIGHTER_HOME/http.log"; then
	code="$($D run --rm curlimages/curl:8.11.1 -sS -o /dev/null -w '%{http_code}' --max-time 10 http://host.docker.internal:18099/ 2>"$LIGHTER_HOME/http-client.log")"
	[ "$code" = 200 ] && pass "host.docker.internal reaches a server on the Mac" || { fail "host.docker.internal: http_code=${code:-none}"; cat "$LIGHTER_HOME/http-client.log"; }
	# Compose's extra_hosts: ["name:host-gateway"] must mean the same Mac, and
	# only it: a second record for the guest's bridge is one a client may try first.
	out="$($D run --rm --add-host probe.test:host-gateway alpine:3.21 sh -c '
		getent hosts probe.test | awk "{print \$1}" | tr "\n" " "
		wget -q -T 10 -O - http://probe.test:18099/' 2>"$LIGHTER_HOME/host-gateway.log")"
	[ "$out" = "192.168.127.254 ok" ] && pass "host-gateway reaches a server on the Mac" || { fail "host-gateway: '${out}'"; cat "$LIGHTER_HOME/host-gateway.log"; }
else
	fail "the host HTTP fixture did not start"
	cat "$LIGHTER_HOME/http.log"
fi

# published: answers while running, gone when stopped
$D run -d --rm --name m3s-http -p 18098:80 alpine:3.21 sh -c 'while true; do printf "HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok" | nc -l -p 80; done' >/dev/null 2>&1
sleep 2
code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 http://127.0.0.1:18098/ 2>/dev/null)"
[ "$code" = 200 ] && pass "a published port answers on the Mac" || fail "published: http_code=${code:-none}"
$D stop -t 1 m3s-http >/dev/null 2>&1; sleep 2
if nc -z -w 1 127.0.0.1 18098 2>/dev/null; then fail "the published port is still open after the container stopped"; else pass "the published port closed with the container"; fi

# published over ::1: `localhost` on a Mac is ::1 first, and Docker's v6
# mapping of a publish reaches the container's IPv6 address, where a server
# that binds 0.0.0.0 (most of them) does not listen. The agent retries a
# refused v6 publish on the guest's v4, Docker's v4 mapping, so the server
# answers on localhost as it would under Docker Desktop.
$D run -d --rm --name m3s-v4only -p 18093:80 alpine:3.21 sh -c 'apk add -q python3 >/dev/null 2>&1; python3 -c "import http.server as h, socketserver as s
s.TCPServer.allow_reuse_address = True
class Ok(h.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.send_header(\"Content-Length\", \"2\"); self.end_headers(); self.wfile.write(b\"ok\")
    def log_message(self, *a): pass
s.TCPServer((\"0.0.0.0\", 80), Ok).serve_forever()"' >/dev/null 2>&1
waited=0; until [ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 http://127.0.0.1:18093/ 2>/dev/null)" = 200 ] || [ "$waited" -ge 60 ]; do sleep 2; waited=$((waited + 2)); done
code4="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 http://127.0.0.1:18093/ 2>/dev/null)"
code6="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 'http://[::1]:18093/' 2>/dev/null)"
if [ "$code4" = 200 ] && [ "$code6" = 200 ]; then
	pass "a server bound to 0.0.0.0 answers on 127.0.0.1 and on ::1 (localhost)"
else
	fail "published over ::1: 127.0.0.1 gave ${code4:-none}, ::1 gave ${code6:-none} after ${waited}s"
	echo "    container: $($D inspect -f '{{.State.Status}} exit={{.State.ExitCode}}' m3s-v4only 2>&1 | head -1)"
	$D logs m3s-v4only 2>&1 | tail -3 | sed 's/^/    /'
fi
# The same server answers and closes at once (HTTP/1.0), which races the
# join: 0.10.4's first event loop attached the vsock end first, and a
# container socket already closing then took the stream down with it, an
# empty reply to one request in twenty.
empty=0
for _ in $(seq 1 200); do
	[ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 http://127.0.0.1:18093/ 2>/dev/null)" = 200 ] || empty=$((empty + 1))
done
[ "$empty" -eq 0 ] && pass "200 requests to a server that answers and closes, every one answered" || fail "$empty of 200 requests to a server that answers and closes got no answer"
$D stop -t 1 m3s-v4only >/dev/null 2>&1

# integrity: a checksummed quarter gigabyte each way. iperf3 checks
# nothing about the bytes it moves, and a split copy once reordered them.
python3 - <<'PY' &
import socket, hashlib
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); srv.bind(("0.0.0.0", 18095)); srv.listen(2)
c, _ = srv.accept(); h = hashlib.sha256(); buf = bytearray(1 << 20)
while True:
    n = c.recv_into(buf)
    if not n: break
    h.update(buf[:n])
open("/tmp/m3s-out.sha", "w").write(h.hexdigest())
PY
SUM_PID=$!
sleep 1
out="$($D run --rm alpine:3.21 sh -c 'head -c 268435456 /dev/urandom > /tmp/x && sha256sum /tmp/x | cut -c1-64 && nc -w 5 '"$LAN_IP"' 18095 < /tmp/x' 2>/dev/null | tail -1)"
wait "$SUM_PID" 2>/dev/null
[ -n "$out" ] && [ "$out" = "$(cat /tmp/m3s-out.sha 2>/dev/null)" ] && pass "256 MiB out of a container arrived intact" || fail "integrity out: sent ${out:-nothing}, got $(cat /tmp/m3s-out.sha 2>/dev/null)"
$D run -d --rm --name m3s-sink -p 18094:9 alpine:3.21 sh -c 'nc -l -p 9 > /tmp/y; sha256sum /tmp/y | cut -c1-64 > /tmp/y.sha; sleep 30' >/dev/null 2>&1
sleep 2
inhash="$(python3 -c '
import socket, hashlib, os
s = socket.create_connection(("127.0.0.1", 18094)); h = hashlib.sha256()
for _ in range(256):
    b = os.urandom(1 << 20); h.update(b); s.sendall(b)
s.close(); print(h.hexdigest())')"
sleep 3
got="$($D exec m3s-sink cat /tmp/y.sha 2>/dev/null)"
[ -n "$got" ] && [ "$got" = "$inhash" ] && pass "256 MiB into a published port arrived intact" || fail "integrity in: sent $inhash, got ${got:-nothing}"
$D rm -f m3s-sink >/dev/null 2>&1

# overlap: a retransmitted segment that starts inside delivered bytes is
# read once by the kernel join (a quarter gigabyte once arrived with four
# segments repeated in place, and the check above passed dozens of times
# between). A raw client behind the container's egress sends sixteen bytes,
# then sixteen more from eight bytes back; the Mac must receive twenty-four.
$D run -d --name m3s-overlap --cap-add NET_ADMIN alpine:3.21 sleep 300 >/dev/null 2>&1
$D exec m3s-overlap apk add -q python3 iptables >/dev/null 2>&1
$D cp scripts/gates/fixtures/overlap-client.py m3s-overlap:/overlap-client.py >/dev/null 2>&1
got="$(python3 - "$LAN_IP" $D <<'PY'
import socket, subprocess, sys, threading
lan, docker = sys.argv[1], sys.argv[2:]
srv = socket.socket(); srv.bind(("0.0.0.0", 0)); srv.listen(1); srv.settimeout(30); port = srv.getsockname()[1]
chunks, first = [], threading.Event()
def receive():
    try:
        with srv, srv.accept()[0] as c:
            c.settimeout(10)
            while True:
                b = c.recv(65536)
                if not b: break
                chunks.append(b)
                if sum(map(len, chunks)) >= 16: first.set()
    except OSError: pass
t = threading.Thread(target=receive); t.start()
p = subprocess.Popen(docker + ["exec", "-i", "m3s-overlap", "python3", "/overlap-client.py", lan, str(port)],
                     stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, text=True)
if first.wait(20): p.communicate("next\n", timeout=30)
else: p.kill()
t.join(30)
print(b"".join(chunks).decode("ascii", "replace"))
PY
)"
$D rm -f m3s-overlap >/dev/null 2>&1
[ "$got" = ABCDEFGHIJKLMNOPQRSTUVWX ] && pass "a segment overlapping delivered bytes is read once" || fail "overlap: got ${got:-nothing}, want ABCDEFGHIJKLMNOPQRSTUVWX"

# many: a thousand concurrent connections to a holder on the Mac
python3 - <<'PY' &
import socket, threading
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); srv.bind(("0.0.0.0", 18097)); srv.listen(2048)
held = []
while True:
    c, _ = srv.accept(); held.append(c)
PY
HOLD_PID=$!
sleep 1
count="$($D run --rm alpine:3.21 sh -c 'apk add -q python3 >/dev/null 2>&1; python3 -c "
import socket
ok=0; held=[]
for i in range(1000):
    try:
        s=socket.create_connection((\"'"$LAN_IP"'\", 18097), 5); held.append(s); ok+=1
    except Exception as e: pass
print(ok)"' 2>/dev/null | tail -1)"
[ "${count:-0}" -ge 1000 ] && pass "a thousand concurrent connections from one container" || fail "many: only ${count:-0} of 1000 connected"

# dns and icmp stay on the network device
$D run --rm alpine:3.21 nslookup example.com >/dev/null 2>&1 && pass "DNS from a container" || fail "DNS failed"
# a resolver of the container's own is a UDP flow like any other (port 53
# was once exempt from the divert, and those datagrams were dropped)
got="$($D run --rm alpine:3.21 sh -c 'apk add -q bind-tools >/dev/null 2>&1 && dig +time=5 +tries=1 +short @8.8.8.8 example.com A' 2>/dev/null | grep -c '^[0-9]')"
[ "${got:-0}" -gt 0 ] && pass "UDP to an external resolver (dig @8.8.8.8)" || fail "external resolver over UDP: dig got nothing"
$D run --rm alpine:3.21 ping -c 1 -W 3 1.1.1.1 >/dev/null 2>&1 && pass "ICMP from a container" || fail "ping failed"

# ipv6: a container has an address and a default route of its own either
# way; with a v6 route on the Mac a v6 destination is reached over TCP, UDP
# and ICMP and AAAA is answered, without one AAAA is withheld so nothing
# tries v6 first.
route="$($D run --rm alpine:3.21 ip -6 route show default 2>/dev/null)"
[ -n "$route" ] && pass "a container has a v6 default route" || fail "IPv6: no v6 default route in a container"
aaaa="$($D run --rm alpine:3.21 nslookup -type=AAAA example.com 2>/dev/null | grep -cE '^Address: .*:.*:')"
# macOS curl -6 can connect to an IPv4-mapped address (::ffff:...), so its
# success does not establish a v6 route. Probe a literal v6 destination,
# as the resolver does; connect on a UDP socket sends no packet.
if python3 - <<'PY'
import socket, sys
try:
    with socket.socket(socket.AF_INET6, socket.SOCK_DGRAM) as s:
        s.connect(("2001:4860:4860::8888", 53))
except OSError:
    sys.exit(1)
PY
then
	code="$($D run --rm curlimages/curl:8.11.1 -6 -s -o /dev/null -w '%{http_code}' --max-time 15 https://example.com 2>/dev/null)"
	[ "$code" = 200 ] && pass "IPv6 destination over the stream" || fail "IPv6: http_code=${code:-none}"
	[ "${aaaa:-0}" -gt 0 ] && pass "AAAA answered on a Mac with a v6 route" || fail "IPv6: no AAAA for example.com"
	got="$($D run --rm alpine:3.21 sh -c 'apk add -q bind-tools >/dev/null 2>&1 && dig -6 +time=5 +tries=1 +short @2001:4860:4860::8888 example.com A' 2>/dev/null | grep -c '^[0-9]')"
	[ "${got:-0}" -gt 0 ] && pass "UDP over IPv6 (dig to a v6 resolver)" || fail "IPv6 UDP: dig over v6 got nothing"
	$D run --rm alpine:3.21 ping -6 -c 1 -W 3 2606:4700:4700::1111 >/dev/null 2>&1 && pass "ICMPv6 from a container" || fail "ping6 failed"
else
	echo "  ··   IPv6: the Mac has no v6 route; egress checks skipped"
	[ "${aaaa:-0}" -eq 0 ] && pass "AAAA withheld on a Mac without a v6 route" || fail "IPv6: AAAA answered with no v6 route on the Mac"
fi

# The outbound proxy under a thread shortage. On 2026-09-29 the guest ran
# out of threads (threads-max is set at boot from the RAM then, 2 GiB in
# cooperative mode) and the proxy, a thread per connection, panicked on the
# spawn and never came back: every container's TCP was refused until a
# restart. Here the shortage is made on purpose: threads-max is set just
# above the guest's count while one process opens connections that stay
# open, then put back, and egress must still work.
echo "==> The TCP proxy under a thread shortage"
guest_sh() {
	python3 - "$1" <<'PY'
import os, socket, sys
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as control:
    control.settimeout(30)
    control.connect(os.path.join(os.environ['LIGHTER_HOME'], 'control.sock'))
    control.sendall(b'sh ' + sys.argv[1].encode() + b'\n')
    response = b''
    while not response.endswith(b'--end--\n'):
        chunk = control.recv(65536)
        if not chunk:
            break
        response += chunk
lines = response.decode(errors='replace').splitlines()
print('\n'.join(l for l in lines if l != '--end--' and not l.startswith('exit=')))
PY
}
tcp_proxy_alive() { guest_sh 'for p in $(pidof lighter-agent); do tr "\0" " " </proc/$p/cmdline; echo; done' | grep -q -- '--tcp-proxy'; }
python3 - <<'PY' &
import socket
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", 18094)); srv.listen(1024)
held = []
while True:
    held.append(srv.accept()[0])
PY
FLOOD_HOLD_PID=$!
sleep 1
$D run -d --name m3s-flood node:24-alpine node -e '
const net = require("net");
setTimeout(() => {
  let open = 0, refused = 0, reset = 0;
  for (let i = 0; i < 300; i++) {
    const s = net.connect(18094, "'"$LAN_IP"'");
    let connected = false;
    s.on("connect", () => { connected = true; open++; });
    s.once("error", () => { if (connected) { open--; reset++; } else { refused++; } });
  }
  setTimeout(() => { console.log("open " + open + " reset " + reset + " refused " + refused); process.exit(0); }, 12000);
}, 4000);' >/dev/null 2>&1
sleep 1
threads_max="$(guest_sh 'cat /proc/sys/kernel/threads-max')"
tasks="$(guest_sh 'ls -d /proc/[0-9]*/task/[0-9]* | wc -l')"
guest_sh "sysctl -w kernel.threads-max=$((tasks + 40))" >/dev/null
$D wait m3s-flood >/dev/null 2>&1
guest_sh "sysctl -w kernel.threads-max=$threads_max" >/dev/null
flood="$($D logs m3s-flood 2>&1 | tail -1)"
$D rm -f m3s-flood >/dev/null 2>&1
kill "$FLOOD_HOLD_PID" 2>/dev/null
echo "    flood with threads-max at $((tasks + 40)) (from $threads_max): $flood"
if tcp_proxy_alive; then
	pass "the TCP proxy is still running after the shortage"
else
	fail "the TCP proxy died in the shortage: $(grep -m1 -A1 'panicked' "$LIGHTER_HOME/machine.log" | tr '\n' ' ')"
fi
if $D run --rm curlimages/curl:8.11.1 -s -o /dev/null --max-time 10 https://example.com 2>/dev/null; then
	pass "containers reach the network after the shortage"
else
	fail "no container can connect out after the shortage"
fi

echo "==> An agent that dies comes back"
limits="$(guest_sh 'cat /proc/sys/kernel/threads-max /proc/sys/kernel/pid_max' | xargs)"
[ "$limits" = "4194304 4194304" ] && pass "threads-max and pid_max are the kernel's maximums" || fail "limits: threads-max and pid_max read $limits"
proxy_pid="$(guest_sh 'for p in $(pidof lighter-agent); do tr "\0" " " </proc/$p/cmdline | grep -q -- --tcp-proxy && echo $p; done' | head -1)"
guest_sh "kill -9 $proxy_pid" >/dev/null
back=""
for i in $(seq 1 30); do
	if tcp_proxy_alive; then back=$i; break; fi
	sleep 0.1
done
if [ -n "$back" ] && [ "$back" -le 30 ]; then
	pass "the TCP proxy was running again $((back * 100)) ms after a kill -9"
else
	fail "the TCP proxy did not come back within 3 s of a kill -9"
fi
code="$($D run --rm curlimages/curl:8.11.1 -s -o /dev/null -w '%{http_code}' --max-time 15 https://example.com 2>/dev/null)"
[ "$code" = 200 ] && pass "egress works after the restart" || fail "egress after the restart: http_code=${code:-none}"
"$LIGHTER" status 2>/dev/null | grep -q "restarted: tcp-proxy=1" && pass "lighter status reports the restart" || fail "lighter status: $("$LIGHTER" status 2>&1 | grep -i agents || echo 'no agents line')"

echo "==> A stream its server resets is let go (#63)"
# The Mac's end of a joined stream that its server reset reports only RDHUP;
# each such stream kept two descriptors until the proxy ran out of them.
proxy_fds() { guest_sh 'for p in $(pidof lighter-agent); do grep -q -- --tcp-proxy /proc/$p/cmdline && ls /proc/$p/fd | wc -l; done' | head -1; }
python3 - <<'PY' &
import socket, struct, threading
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", 18097)); srv.listen(128)
def serve(c):
    try:
        if c.recv(1) == b"P": c.sendall(b"P")
        c.recv(1)
        c.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
    finally:
        c.close()
while True:
    c, _ = srv.accept(); threading.Thread(target=serve, args=(c,), daemon=True).start()
PY
RESET_PID=$!
sleep 1
before="$(proxy_fds)"
done_n="$($D run --rm node:24-alpine node -e '
const net = require("net");
let n = 0;
function one() {
  if (n === 32) { console.log(n); return; }
  const s = net.connect(18097, "'"$LAN_IP"'", () => s.write("P"));
  s.once("data", () => s.write("X"));
  s.on("error", () => {}); s.on("close", () => { n++; one(); });
}
one();' 2>/dev/null)"
sleep 2
after="$(proxy_fds)"
kill "$RESET_PID" 2>/dev/null
if [ "$done_n" = 32 ] && [ -n "$before" ] && [ "$after" = "$before" ]; then
	pass "32 streams their server reset left no descriptor behind ($before before and after)"
else
	fail "after ${done_n:-0} reset streams the proxy holds ${after:-?} descriptors, ${before:-?} before"
fi

# Streams in their thousands on every route: containers' connections out,
# the Mac's in through a published port, and `docker logs -f` through the
# Docker socket. Every connection is held open, the three agents' thread
# counts must not move, and egress must keep working. Out and in run one
# after the other, since both draw on the Mac's 16,384 ephemeral ports; the
# outbound connections go to two ports on the Mac for the same reason. Each
# stream is a socket in the VMM, so the floods are sized to what this Mac
# lets one process hold: 20,000 and 10,000 on the M5, about 4,000 on the M1
# (kern.maxfilesperproc 10,240), where 20,000 took the VMM to its limit and
# its control and Docker sockets stopped answering until the flood ended.
per_process="$(sysctl -n kern.maxfilesperproc)"
room=$(( (per_process - 2000) / 2 ))
flood_out=$(( room < 20000 ? room : 20000 ))
flood_in=$(( room < 10000 ? room : 10000 ))
echo "==> Streams by the thousand ($flood_out out, $flood_in in; this Mac allows $per_process files a process)"
python3 - <<'PY' &
import select, socket
listeners = []
for port in (18094, 18095):
    s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("0.0.0.0", port)); s.listen(4096); s.setblocking(False); listeners.append(s)
held = []
while True:
    for s in select.select(listeners, [], [])[0]:
        try:
            while True:
                held.append(s.accept()[0])
        except BlockingIOError:
            pass
PY
FLOOD_HOLD_PID=$!
agent_threads() {
	guest_sh 'for p in $(pidof lighter-agent); do a=$(tr "\0" " " </proc/$p/cmdline); case "$a" in *--tcp-proxy*|*--inbound*|*/run/docker.sock*) echo "$(ls /proc/$p/task | wc -l)";; esac; done' | xargs
}
egress() { $D run --rm curlimages/curl:8.11.1 -s -o /dev/null -w '%{http_code}' --max-time 15 https://example.com 2>/dev/null; }
before="$(agent_threads)"
tcp_before="$(sysctl -n net.inet.tcp.pcbcount)"
$D run -d --name m3s-flood --ulimit nofile=65536:65536 node:24-alpine node -e '
const net = require("net");
let open = 0, failed = 0; const held = [];
for (let i = 0; i < '"$flood_out"'; i++) {
  const s = net.connect(i % 2 ? 18095 : 18094, "'"$LAN_IP"'");
  s.on("connect", () => { open++; held.push(s); }); s.on("error", () => { failed++; });
}
setTimeout(() => { console.log("open " + open + " failed " + failed); }, 25000);
setTimeout(() => process.exit(0), 32000);' >/dev/null 2>&1
sleep 27
during_out="$(agent_threads)"
code_out="$(egress)"
out="$($D logs m3s-flood 2>&1 | tail -1)"
$D wait m3s-flood >/dev/null 2>&1
$D rm -f m3s-flood >/dev/null 2>&1
kill "$FLOOD_HOLD_PID" 2>/dev/null
# The closed connections hold their ports in TIME_WAIT for about thirty
# seconds on a Mac. Counted by the kernel's TCP control blocks, which include
# TIME_WAIT: macOS 27's netstat lists no TCP sockets at all, so a count read
# from it was always zero and the inbound flood started on spent ports.
for _ in $(seq 1 90); do [ "$(sysctl -n net.inet.tcp.pcbcount)" -lt $(( tcp_before + 2000 )) ] && break; sleep 1; done
echo "    TCP control blocks between the floods: $(sysctl -n net.inet.tcp.pcbcount) (before the floods: $tcp_before)"

$D run -d --name m3s-hold -p 18092:18092 node:24-alpine node -e '
const net = require("net"); const held = [];
net.createServer(s => { held.push(s); s.on("error", () => {}); }).listen(18092);' >/dev/null 2>&1
$D run -d --name m3s-logs alpine:3.21 sh -c 'while true; do echo tick; sleep 1; done' >/dev/null 2>&1
for _ in $(seq 1 30); do nc -z -w 1 127.0.0.1 18092 2>/dev/null && break; sleep 1; done
python3 - "$LIGHTER_HOME" "$flood_in" <<'PY' >"$LIGHTER_HOME/flood-in.log" 2>&1 &
import socket, sys, time
held, failed, errors = [], 0, {}
for i in range(int(sys.argv[2])):
    try:
        held.append(socket.create_connection(("127.0.0.1", 18092), timeout=10))
    except OSError as e:
        failed += 1
        errors[e.strerror] = errors.get(e.strerror, 0) + 1
docker = []
for i in range(200):
    try:
        s = socket.socket(socket.AF_UNIX); s.settimeout(10); s.connect(sys.argv[1] + "/docker.sock")
        s.sendall(b"GET /containers/m3s-logs/logs?follow=1&stdout=1 HTTP/1.1\r\nHost: docker\r\n\r\n")
        if b"200 OK" in s.recv(4096):
            docker.append(s)
    except OSError:
        pass
print("in", len(held), "failed", failed, "logs", len(docker), *(f"({n} {e})" for e, n in errors.items()), flush=True)
time.sleep(30)
PY
FLOOD_IN_PID=$!
for _ in $(seq 1 60); do [ -s "$LIGHTER_HOME/flood-in.log" ] && break; sleep 1; done
during_in="$(agent_threads)"
code_in="$(egress)"
in="$(cat "$LIGHTER_HOME/flood-in.log")"
kill "$FLOOD_IN_PID" 2>/dev/null
$D rm -f m3s-hold m3s-logs >/dev/null 2>&1
echo "    outbound: $out; inbound: $in; agent threads before [$before], out [$during_out], in [$during_in]"
[ "$out" = "open $flood_out failed 0" ] && pass "$flood_out outbound connections open at once" || fail "outbound flood: ${out:-no report}"
[ "$in" = "in $flood_in failed 0 logs 200" ] && pass "$flood_in inbound connections and 200 followed logs at once" || fail "inbound flood: ${in:-no report}"
if [ -n "$before" ] && [ "$before" = "$during_out" ] && [ "$before" = "$during_in" ]; then
	pass "the stream agents' threads did not move ($before)"
else
	fail "agent threads before [$before], out [$during_out], in [$during_in]"
fi
[ "$code_out" = 200 ] && [ "$code_in" = 200 ] && pass "egress works during both floods" || fail "egress during the floods: out ${code_out:-none}, in ${code_in:-none}"

# nothing on eth0 while all that happened, beyond DNS and ICMP
echo
[ "$FAILED" -eq 0 ] && echo "m3-streams: all checks passed" || echo "m3-streams: FAILED"
exit $FAILED
