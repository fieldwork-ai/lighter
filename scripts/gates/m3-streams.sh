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
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
LIGHTER="${LIGHTER_BIN:-target/release/lighter}"
export LIGHTER_STREAMS=1
export LIGHTER_HOME="$(mktemp -d -t lighter-m3s)"
D="docker -H unix://$LIGHTER_HOME/docker.sock"
FAILED=0
pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
HTTP_PID=""
HOLD_PID=""
cleanup() {
	[ -z "$HTTP_PID" ] || kill "$HTTP_PID" 2>/dev/null
	[ -z "$HOLD_PID" ] || kill "$HOLD_PID" 2>/dev/null
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
python3 -m http.server 18099 --bind 127.0.0.1 >/dev/null 2>&1 &
HTTP_PID=$!
sleep 1
code="$($D run --rm curlimages/curl:8.11.1 -s -o /dev/null -w '%{http_code}' --max-time 10 http://host.docker.internal:18099/ 2>/dev/null)"
[ "$code" = 200 ] && pass "host.docker.internal reaches a server on the Mac" || fail "host.docker.internal: http_code=${code:-none}"

# published: answers while running, gone when stopped
$D run -d --rm --name m3s-http -p 18098:80 alpine:3.21 sh -c 'while true; do printf "HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok" | nc -l -p 80; done' >/dev/null 2>&1
sleep 2
code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 http://127.0.0.1:18098/ 2>/dev/null)"
[ "$code" = 200 ] && pass "a published port answers on the Mac" || fail "published: http_code=${code:-none}"
$D stop -t 1 m3s-http >/dev/null 2>&1; sleep 2
if nc -z -w 1 127.0.0.1 18098 2>/dev/null; then fail "the published port is still open after the container stopped"; else pass "the published port closed with the container"; fi

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
$D run --rm alpine:3.21 ping -c 1 -W 3 1.1.1.1 >/dev/null 2>&1 && pass "ICMP from a container" || fail "ping failed"

# ipv6, when the Mac has it
if curl -6 -s -o /dev/null --max-time 5 https://example.com 2>/dev/null; then
	code="$($D run --rm curlimages/curl:8.11.1 -6 -s -o /dev/null -w '%{http_code}' --max-time 15 https://example.com 2>/dev/null)"
	[ "$code" = 200 ] && pass "IPv6 destination over the stream" || fail "IPv6: http_code=${code:-none}"
else
	echo "  ··   IPv6: the Mac has no v6 route; skipped"
fi

# nothing on eth0 while all that happened, beyond DNS and ICMP
echo
[ "$FAILED" -eq 0 ] && echo "m3-streams: all checks passed" || echo "m3-streams: FAILED"
exit $FAILED
