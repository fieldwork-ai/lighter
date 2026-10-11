#!/usr/bin/env bash
# Milestone 3 gate, part five: published ports, every way Docker can publish one.
#
#   lan        — `-p 18098:80` answers on loopback, on the Mac's LAN address,
#                and on its IPv6 addresses (loopback and global)
#   loopback   — `-p 127.0.0.1:18097:80` answers on loopback and nowhere else
#   withdrawn  — every listener is gone once the container is, and the same
#                port publishes again at once
#   udp        — `-p 18094:9/udp` echoes a small and an 8 KiB datagram over
#                v4 and v6, two hundred clients each get their own reply, and
#                the port refuses once the container is gone
#   dns        — a DNS server on `-p 53:53` answers over UDP and TCP, and other
#                containers still resolve through the guest
#   peer       — a published container sees a LAN client as itself, and
#                loopback as the guest
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
LIGHTER="${LIGHTER_BIN:-target/release/lighter}"
export LIGHTER_HOME="$(mktemp -d -t lighter-m3p)"
if [ -n "${LIGHTER_BENCH_OWNER_FILE:-}" ]; then
	python3 - "$LIGHTER_BENCH_OWNER_FILE" "$LIGHTER_HOME/lighter.app/Contents/MacOS/lighter" <<'PYOWNER'
import json, sys
with open(sys.argv[1], 'a') as f:
    f.write(json.dumps(sys.argv[2]) + '\n')
PYOWNER
fi
D="docker -H unix://$LIGHTER_HOME/docker.sock"
FAILED=0
SKIPPED=0
pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
skip() { printf '  \033[33mskip\033[0m %s\n' "$*"; SKIPPED=$((SKIPPED + 1)); }
cleanup() {
	"$LIGHTER" stop >/dev/null 2>&1 || true
	[ -f "$LIGHTER_HOME/lighter.pid" ] && kill -9 "$(cat "$LIGHTER_HOME/lighter.pid")" 2>/dev/null || true
	rm -rf "$LIGHTER_HOME"
}
trap cleanup EXIT

echo "==> Booting"
"$LIGHTER" start >"$LIGHTER_HOME/start.log" 2>&1 &
for _ in $(seq 1 60); do $D info >/dev/null 2>&1 && break; sleep 1; done
$D info >/dev/null 2>&1 || { fail "machine did not come up"; exit 1; }
$D pull -q alpine:3.21 >/dev/null 2>&1
$D pull -q alpine/socat:1.8.0.0 >/dev/null 2>&1

# The card with an address, Ethernet first: the v6 address is taken from the
# same one (it was always en0's, so a Mac on Wi-Fi skipped every v6 check).
LAN_IF=""
for candidate in en0 en1; do
	if ipconfig getifaddr "$candidate" >/dev/null 2>&1; then LAN_IF="$candidate"; break; fi
done
LAN_IP="$([ -n "$LAN_IF" ] && ipconfig getifaddr "$LAN_IF" 2>/dev/null || true)"
# The Mac's stable global v6 address, not a temporary one (those rotate).
V6_IP="$([ -n "$LAN_IF" ] && ifconfig "$LAN_IF" 2>/dev/null | awk '/inet6/ && /autoconf/ && /secured/ && !/temporary/ {print $2; exit}')"
[ -n "$LAN_IP" ] || skip "no LAN address on en0/en1; LAN checks skipped"
[ -n "$V6_IP" ] || skip "no global IPv6 address on ${LAN_IF:-en0}; global v6 checks skipped"

http() { curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$1" 2>/dev/null; }
# The code once the listener is up: a publish takes Docker's event, a
# reconcile and a bind, and a fixed pause raced it once.
wait_http() {
	local code
	for _ in $(seq 1 20); do
		code="$(http "$1")"
		[ "$code" = 200 ] && { echo "$code"; return; }
		sleep 0.5
	done
	echo "$code"
}
serve_http() { # name port-spec
	$D run -d --rm --name "$1" -p "$2" alpine:3.21 sh -c 'while true; do printf "HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok" | nc -l -p 80; done' >/dev/null 2>&1
	sleep 2
}

# lan: a plain publish is on every interface, both families
serve_http m3p-lan 18098:80
code="$(wait_http http://127.0.0.1:18098/)"
[ "$code" = 200 ] && pass "-p 18098:80 answers on 127.0.0.1" || fail "lan: 127.0.0.1 http_code=$code"
[ "$(http 'http://[::1]:18098/')" = 200 ] && pass "-p 18098:80 answers on [::1]" || fail "lan: [::1] http_code=$(http 'http://[::1]:18098/')"
if [ -n "$LAN_IP" ]; then
	[ "$(http "http://$LAN_IP:18098/")" = 200 ] && pass "-p 18098:80 answers on the LAN address $LAN_IP" || fail "lan: $LAN_IP http_code=$(http "http://$LAN_IP:18098/")"
fi
if [ -n "$V6_IP" ]; then
	[ "$(http "http://[$V6_IP]:18098/")" = 200 ] && pass "-p 18098:80 answers on the global v6 address" || fail "lan: [$V6_IP] http_code=$(http "http://[$V6_IP]:18098/")"
fi

# loopback: a publish with its own address is bound there and nowhere else
serve_http m3p-lo 127.0.0.1:18097:80
code="$(wait_http http://127.0.0.1:18097/)"
[ "$code" = 200 ] && pass "-p 127.0.0.1:18097:80 answers on 127.0.0.1" || fail "loopback: http_code=$code"
if [ -n "$LAN_IP" ]; then
	code="$(http "http://$LAN_IP:18097/")"
	[ "$code" = 000 ] && pass "-p 127.0.0.1:18097:80 is not on the LAN address" || fail "loopback: reachable on $LAN_IP (http_code=$code)"
fi
code="$(http 'http://[::1]:18097/')"
[ "$code" = 000 ] && pass "-p 127.0.0.1:18097:80 is not on [::1]" || fail "loopback: reachable on [::1] (http_code=$code)"

# withdrawn: everything closes with the container, and the port is free again
$D rm -f m3p-lan m3p-lo >/dev/null 2>&1; sleep 2
open=""
for target in 127.0.0.1:18098 [::1]:18098 127.0.0.1:18097; do
	code="$(http "http://$target/")"
	[ "$code" = 000 ] || open="$open $target"
done
[ -z "$open" ] && pass "every listener closed with its container" || fail "still open after the containers stopped:$open"
serve_http m3p-again 18098:80
code="$(wait_http http://127.0.0.1:18098/)"
[ "$code" = 200 ] && pass "the same port publishes again at once" || fail "republish: http_code=$code"
$D rm -f m3p-again >/dev/null 2>&1

# refused: a port the Mac will not give lighter on one family is open on
# neither, says so in `lighter status`, and opens by itself once the holder
# lets go, with no container event to prompt it. Tailscale Serve holding the
# port on a tailnet address did this to MinIO: `[::]:9000` answered and
# `0.0.0.0:9000` did not, until some other container happened to start.
python3 -c 'import socket, time
s = socket.socket(); s.bind(("0.0.0.0", 18096)); s.listen(); time.sleep(20)' &
HOLDER=$!
sleep 1
serve_http m3p-held 18096:80
listed="$("$LIGHTER" status 2>/dev/null | grep -c 'tcp 18096 not forwarded')"
[ "$listed" = 1 ] && [ "$(http 'http://[::1]:18096/')" = 000 ] \
	&& pass "a port refused on v4 is closed on v6 too, and lighter status names it" \
	|| fail "refused port: [::1] http_code=$(http 'http://[::1]:18096/'), status listed it $listed times"
wait "$HOLDER" 2>/dev/null
# The retry backs off to thirty seconds; a minute covers it.
for _ in $(seq 1 60); do
	code="$(http http://127.0.0.1:18096/)"
	[ "$code" = 200 ] && break
	sleep 1
done
[ "$code" = 200 ] && [ "$(http 'http://[::1]:18096/')" = 200 ] \
	&& [ "$("$LIGHTER" status 2>/dev/null | grep -c 'not forwarded')" = 0 ] \
	&& pass "it opened on both families by itself once the holder let go" \
	|| fail "after the holder let go: 127.0.0.1 http_code=$code, [::1] $(http 'http://[::1]:18096/')"
$D rm -f m3p-held >/dev/null 2>&1

# udp: an echo on a published UDP port
$D run -d --rm --name m3p-udp -p 18094:9/udp alpine/socat:1.8.0.0 UDP6-RECVFROM:9,ipv6only=0,fork EXEC:cat >/dev/null 2>&1
sleep 2
udp_echo() { # host size -> "ok" or a reason
	python3 - "$1" "$2" "${3:-18094}" <<'PY'
import socket, sys, os
host, size, port = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
fam = socket.AF_INET6 if ":" in host else socket.AF_INET
s = socket.socket(fam, socket.SOCK_DGRAM); s.settimeout(5)
payload = os.urandom(size)
try:
    s.sendto(payload, (host, port))
    got, _ = s.recvfrom(65536)
    print("ok" if got == payload else f"mismatch: {len(got)} bytes back for {size}")
except Exception as e:
    print(f"{type(e).__name__}: {e}")
PY
}
for host in 127.0.0.1 ::1 ${LAN_IP:-} ${V6_IP:-}; do
	for size in 64 8192; do
		r="$(udp_echo "$host" "$size")"
		[ "$r" = ok ] && pass "udp: $size B echoed via $host" || fail "udp: $size B via $host: $r"
	done
done
many="$(python3 - <<'PY'
import socket, os, select, time
socks = []
for i in range(200):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.setblocking(False)
    s.sendto(i.to_bytes(2, "big") + os.urandom(30), ("127.0.0.1", 18094)); socks.append(s)
# One deadline for all of them, not one per socket.
ok, pending, deadline = 0, {s.fileno(): (i, s) for i, s in enumerate(socks)}, time.time() + 10
while pending and time.time() < deadline:
    ready, _, _ = select.select([s for _, s in pending.values()], [], [], 0.5)
    for s in ready:
        i, _ = pending.pop(s.fileno())
        got, _ = s.recvfrom(65536)
        ok += int.from_bytes(got[:2], "big") == i
print(ok)
PY
)"
[ "${many:-0}" = 200 ] && pass "udp: two hundred clients each got their own reply" || fail "udp: only ${many:-0} of 200 clients got their own reply"
$D rm -f m3p-udp >/dev/null 2>&1; sleep 2
r="$(udp_echo 127.0.0.1 64)"
case "$r" in ok) fail "udp: the port still echoes after the container stopped" ;; *) pass "udp: the port is closed with the container ($r)" ;; esac

# A v6 wildcard publication with NO v4 sibling must dial a v6 guest
# destination. Merely accepting v6 on the Mac and dialing the guest's v4
# interface would pass the dual-stack checks above but fails this service.
$D run -d --rm --name m3p-udp6 -p '[::]:18096:9/udp' alpine/socat:1.8.0.0 UDP6-RECVFROM:9,ipv6only=1,fork EXEC:cat >/dev/null 2>&1
sleep 2
r="$(udp_echo ::1 64 18096)"
[ "$r" = ok ] && pass "IPv6-only UDP publish reaches an IPv6-only service" || fail "IPv6-only UDP: $r"
$D rm -f m3p-udp6 >/dev/null 2>&1

echo "==> A DNS server published on port 53"
# The guest's own resolver once held port 53 and nothing could publish it,
# Pi-hole included (0.10.2). The container answers a name of its own; any
# other container must still resolve through the guest, not through it.
# `--interface`: without one dnsmasq answers only its own subnet, and a LAN
# client now arrives as itself (Pi-hole's "local" listening mode is the same).
$D run -d --name m3p-dns -p 53:53/udp -p 53:53/tcp alpine:3.21 sh -c \
	'apk add -q dnsmasq >/dev/null 2>&1 && exec dnsmasq -k -u root --no-resolv --interface=eth0 --address=/lighter.test/10.9.8.7' >/dev/null 2>&1 \
	|| fail "port 53 could not be published"
for _ in $(seq 1 40); do [ "$(dig +short +time=1 +tries=1 @127.0.0.1 lighter.test 2>/dev/null)" = 10.9.8.7 ] && break; sleep 0.5; done
for via in 127.0.0.1 ${LAN_IP:+"$LAN_IP"}; do
	for mode in notcp tcp; do
		a="$(dig +short +$mode +time=2 +tries=1 @"$via" lighter.test 2>/dev/null)"
		[ "$a" = 10.9.8.7 ] && pass "dns on 53: answered over $([ "$mode" = tcp ] && echo tcp || echo udp) via $via" || fail "dns on 53 via $via ($mode): '${a:-no answer}'"
	done
done
own="$($D run --rm alpine:3.21 sh -c 'nslookup lighter.test 2>&1; nslookup example.com 2>&1' 2>&1)"
if echo "$own" | grep -q 10.9.8.7; then
	fail "another container resolved through the published server"
elif echo "$own" | grep -A3 "example.com" | grep -q "Address"; then
	pass "other containers still resolve through the guest"
else
	fail "a container could not resolve example.com beside it"
fi
$D rm -f m3p-dns >/dev/null 2>&1
sleep 1
[ "$(dig +short +time=1 +tries=1 @127.0.0.1 lighter.test 2>/dev/null)" != 10.9.8.7 ] \
	&& pass "port 53 closed with its container" || fail "port 53 still answers after its container"

echo "==> Who a published container sees"
# A client's own address, passed through the stream header and bound
# transparently in the guest (0.10.2): the LAN address arrives as itself,
# loopback as the guest.
$D run -d --name m3p-peer-tcp -p 18095:9 alpine/socat:1.8.0.0 TCP4-LISTEN:9,fork,reuseaddr SYSTEM:'echo $SOCAT_PEERADDR' >/dev/null 2>&1
$D run -d --name m3p-peer-udp -p 18095:9/udp alpine/socat:1.8.0.0 UDP4-RECVFROM:9,fork SYSTEM:'echo $SOCAT_PEERADDR' >/dev/null 2>&1
sleep 2
if [ -n "$LAN_IP" ]; then
	seen="$(nc -w 3 "$LAN_IP" 18095 </dev/null 2>/dev/null)"
	[ "$seen" = "$LAN_IP" ] && pass "tcp: a LAN client is seen as itself ($seen)" || fail "tcp: a LAN client was seen as '${seen:-nothing}'"
	seen="$(echo x | nc -u -w 2 "$LAN_IP" 18095 2>/dev/null)"
	[ "$seen" = "$LAN_IP" ] && pass "udp: a LAN client is seen as itself ($seen)" || fail "udp: a LAN client was seen as '${seen:-nothing}'"
fi
seen="$(nc -w 3 127.0.0.1 18095 </dev/null 2>/dev/null)"
[ -n "$seen" ] && [ "$seen" != "${LAN_IP:-none}" ] && pass "tcp: loopback is seen as the guest ($seen)" || fail "tcp: loopback was seen as '${seen:-nothing}'"
$D rm -f m3p-peer-tcp m3p-peer-udp >/dev/null 2>&1

echo "==> A host-network container's ports, forwarded from the Mac (#60)"
# Nothing publishes a host-network container's port: Docker has no binding
# to report, so it was reachable from nothing on the Mac. The agent finds
# what such a container listens on, the kernel ringing a doorbell when a
# listener comes or goes, and the Mac forwards it as it does a published
# port.
MAC_LAN="$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || true)"
$D pull -q python:3.12-slim >/dev/null 2>&1
$D rm -f m3-hostweb m3-hostlo m3-hostudp >/dev/null 2>&1
$D run -d --name m3-hostweb --network host python:3.12-slim python3 -m http.server 18123 >/dev/null
$D run -d --name m3-hostlo --network host python:3.12-slim python3 -m http.server 18126 --bind 127.0.0.1 >/dev/null
$D run -d --name m3-hostudp --network host python:3.12-slim python3 -c '
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.bind(("0.0.0.0", 15999))
while True:
    d, a = s.recvfrom(2048); s.sendto(b"echo " + d, a)' >/dev/null
code=000
for _ in $(seq 1 40); do
	code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 http://127.0.0.1:18123/ || true)"
	[ "$code" = 200 ] && break
	sleep 0.5
done
[ "$code" = 200 ] && pass "a host-network server is reachable on the Mac's localhost" \
	|| fail "a host-network server on 18123 is not reachable from the Mac ($code)"
if [ -n "$MAC_LAN" ]; then
	code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://$MAC_LAN:18123/" || true)"
	[ "$code" = 200 ] && pass "and at the Mac's own address, where the container sees the Mac as its client" \
		|| fail "a host-network server is not reachable at $MAC_LAN ($code)"
	sleep 1
	$D logs m3-hostweb 2>&1 | grep -q "^$MAC_LAN " \
		|| fail "the host-network server did not see $MAC_LAN as its client: $($D logs m3-hostweb 2>&1 | grep -oE '^[0-9.]+ ' | sort -u | tr '\n' ' ')"
fi
code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 http://127.0.0.1:18126/ || true)"
lan_code="000"
[ -n "$MAC_LAN" ] && lan_code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://$MAC_LAN:18126/" || true)"
[ "$code" = 200 ] && [ "$lan_code" = 000 ] && pass "a host-network listener on 127.0.0.1 is the Mac's loopback only" \
	|| fail "a loopback listener: localhost $code, the Mac's address $lan_code"
reply="$(python3 -c '
import socket
for _ in range(20):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(0.5)
    s.sendto(b"ping", ("127.0.0.1", 15999))
    try:
        print(s.recvfrom(2048)[0].decode()); break
    except OSError:
        pass')"
[ "$reply" = "echo ping" ] && pass "a host-network UDP service answers from the Mac" \
	|| fail "a host-network UDP echo on 15999 answered: ${reply:-nothing}"
$D rm -f m3-hostweb >/dev/null
gone=1
for _ in $(seq 1 20); do
	curl -s -o /dev/null --max-time 1 http://127.0.0.1:18123/ || { gone=0; break; }
	sleep 0.5
done
[ "$gone" = 0 ] && pass "the forward is withdrawn when the container stops" \
	|| fail "18123 still answers on the Mac after its container stopped"
$D rm -f m3-hostlo m3-hostudp >/dev/null 2>&1

# How long from a server's listen() to its port answering on the Mac, and
# from its close to the port going: a server waits for a nudge (UDP, already
# forwarded), then listens, takes one connection and closes. Polled once a
# second this was up to a second; the doorbell makes it a scan and a line.
$D rm -f m3-hostlat >/dev/null 2>&1
$D run -d --name m3-hostlat --network host python:3.12-slim python3 -c '
import socket
u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); u.bind(("0.0.0.0", 15998))
while True:
    _, a = u.recvfrom(64)
    t = socket.socket(); t.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    t.bind(("0.0.0.0", 18127)); t.listen()
    u.sendto(b"listening", a)
    c, _ = t.accept(); c.close(); t.close()' >/dev/null
latency="$(python3 - <<'PYEOF'
import socket, statistics, time
def nudge():
    for _ in range(400):
        u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); u.settimeout(0.05)
        t0 = time.monotonic(); u.sendto(b"go", ("127.0.0.1", 15998))
        try:
            u.recvfrom(64); return t0
        except OSError:
            pass
    raise SystemExit("no answer to the nudge")
def until(want, start):
    while time.monotonic() - start < 5:
        s = socket.socket(); s.settimeout(0.5)
        ok = s.connect_ex(("127.0.0.1", 18127)) == 0
        s.close()
        if ok == want:
            return (time.monotonic() - start) * 1000
        time.sleep(0.002)
    return 5000
up, down = [], []
for round in range(8):
    t0 = nudge()
    ms = until(True, t0)
    t1 = time.monotonic()
    gone = until(False, t1)
    if round:
        up.append(ms); down.append(gone)
print(f"{statistics.median(up):.0f} {max(up):.0f} {statistics.median(down):.0f} {max(down):.0f}")
PYEOF
)"
read -r up_med up_max down_med down_max <<<"${latency:-5000 5000 5000 5000}"
[ "${up_med:-5000}" -lt 150 ] 2>/dev/null && pass "a listen is forwarded in ${up_med} ms (median of 7, at most ${up_max})" \
	|| fail "a listen took ${up_med:-?} ms to be forwarded (median of 7, at most ${up_max:-?}): ${latency:-no measurement}"
[ "${down_med:-5000}" -lt 250 ] 2>/dev/null && pass "and withdrawn ${down_med} ms after it closes (at most ${down_max})" \
	|| fail "a closed listener took ${down_med:-?} ms to be withdrawn (at most ${down_max:-?})"
$D rm -f m3-hostlat >/dev/null 2>&1

echo "==> A host-network container's localhost is the Mac's too"
# A port nothing in the guest listens on is the Mac's own loopback, both
# families; one a host-network container listens on stays in the guest; a
# bridge container's localhost is its own.
$D run -d --name m3-hostlo2 --network host python:3.12-slim python3 -m http.server 18127 --bind 127.0.0.1 >/dev/null
LO_FILE="$(mktemp -t lighter-m3-lo)"
python3 - "$LO_FILE" >/dev/null 2>&1 <<'PY' &
import http.server, socket, sys, threading
class V6(http.server.HTTPServer):
    address_family = socket.AF_INET6
class Mac(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.end_headers(); self.wfile.write(b"mac")
    def log_message(self, *a): pass
v4 = http.server.HTTPServer(("127.0.0.1", 0), Mac)
v6 = V6(("::1", 0), Mac)
open(sys.argv[1], "w").write(f"{v4.server_port} {v6.server_port}")
threading.Thread(target=v6.serve_forever, daemon=True).start()
v4.serve_forever()
PY
LOSRV=$!
for _ in $(seq 1 50); do [ -s "$LO_FILE" ] && break; sleep 0.1; done
read -r lo4 lo6 < "$LO_FILE"
rm -f "$LO_FILE"
got="$($D run --rm --network host alpine:3.21 wget -q -T 5 -O - "http://127.0.0.1:$lo4/" 2>/dev/null)"
[ "$got" = mac ] && pass "a host-network container reaches the Mac's 127.0.0.1:$lo4" || fail "127.0.0.1:$lo4 from a host-network container: '${got}'"
got="$($D run --rm --network host alpine:3.21 wget -q -T 5 -O - "http://[::1]:$lo6/" 2>/dev/null)"
[ "$got" = mac ] && pass "and the Mac's [::1]:$lo6" || fail "[::1]:$lo6 from a host-network container: '${got}'"
got=""
for _ in $(seq 1 20); do
	got="$($D run --rm --network host alpine:3.21 wget -q -T 5 -O - "http://127.0.0.1:18127/" 2>/dev/null | head -c 15)"
	[ -n "$got" ] && break
	sleep 0.5
done
[ "$got" = "<!DOCTYPE HTML>" ] && pass "a port a host-network container listens on stays in the guest" || fail "127.0.0.1:18127 did not reach the guest's own server: '${got}'"
$D rm -f m3-hostlo2 >/dev/null 2>&1
# Nobody listens on this port, in the guest or on the Mac: refused, as it
# would be on the Mac, not accepted and then dropped, which a port check
# (`nc -z`, a wait-for-it loop) would read as up.
free="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
got="$($D run --rm --network host python:3.12-slim python3 -c "
import socket
for host in ('127.0.0.1', '::1'):
    s = socket.socket(socket.AF_INET6 if ':' in host else socket.AF_INET)
    s.settimeout(5)
    try:
        s.connect((host, $free)); print('accepted', end=' ')
    except ConnectionRefusedError:
        print('refused', end=' ')
    except Exception as e:
        print(type(e).__name__, end=' ')
" 2>&1)"
[ "$got" = "refused refused " ] && pass "a localhost port nobody listens on is refused, both families" \
	|| fail "a localhost port nobody listens on: ${got:-nothing} (wanted refused, refused)"
# UDP: a datagram to the Mac's loopback is answered from there; one to a
# port nobody has bound is refused (port unreachable, a connected socket's
# ECONNREFUSED); one to a socket in the guest stays there.
UDP_FILE="$(mktemp -t lighter-m3-udp)"
python3 - "$UDP_FILE" >/dev/null 2>&1 <<'PY' &
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(("127.0.0.1", 0))
open(sys.argv[1], "w").write(str(s.getsockname()[1]))
while True:
    data, peer = s.recvfrom(2048)
    s.sendto(b"mac:" + data, peer)
PY
UDPSRV=$!
for _ in $(seq 1 50); do [ -s "$UDP_FILE" ] && break; sleep 0.1; done
udp_port="$(cat "$UDP_FILE")"; rm -f "$UDP_FILE"
udp_free="$(python3 -c 'import socket; s=socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
got="$($D run --rm --network host python:3.12-slim python3 -c "
import socket
c = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); c.settimeout(5)
c.sendto(b'hi', ('127.0.0.1', $udp_port))
try: print(c.recvfrom(100)[0].decode(), end=' ')
except Exception as e: print(type(e).__name__, end=' ')
d = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); d.settimeout(5); d.connect(('127.0.0.1', $udp_free)); d.send(b'x')
try: d.recv(10); print('answered', end=' ')
except ConnectionRefusedError: print('refused', end=' ')
except Exception as e: print(type(e).__name__, end=' ')
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.bind(('127.0.0.1', 0)); s.settimeout(5)
c.sendto(b'here', s.getsockname())
try: print(s.recvfrom(100)[0].decode())
except Exception as e: print(type(e).__name__)
" 2>&1)"
[ "$got" = "mac:hi refused here" ] && pass "UDP too: the Mac's loopback answers, a closed port is refused, a guest socket keeps its own" \
	|| fail "UDP to localhost from a host-network container: '${got}' (wanted 'mac:hi refused here')"
kill "$UDPSRV" 2>/dev/null; wait "$UDPSRV" 2>/dev/null
if $D run --rm alpine:3.21 wget -q -T 3 -O - "http://127.0.0.1:$lo4/" >/dev/null 2>&1; then
	fail "a bridge container reached the Mac's loopback through its own localhost"
else
	pass "a bridge container's localhost is still its own"
fi
kill "$LOSRV" 2>/dev/null; wait "$LOSRV" 2>/dev/null

echo "==> A port published on one of the Mac's addresses"
# `-p 192.168.1.20:9000:9000`: dockerd binds an address the guest does not
# have, and the container never started (MinIO, 0.12.1). Docker Desktop and
# OrbStack bind exactly that address on the Mac. It must answer there, not on
# loopback, and a container must still reach a Mac service on that address.
if [ -n "$LAN_IP" ]; then
	$D rm -f m3-onmac m3-onmac-udp >/dev/null 2>&1
	if $D run -d --name m3-onmac -p "$LAN_IP:18140:8000" python:3.12-slim python3 -m http.server 8000 >/dev/null 2>"$LIGHTER_HOME/onmac.err"; then
		code=000
		for _ in $(seq 1 20); do
			code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "http://$LAN_IP:18140/" || true)"
			[ "$code" = 200 ] && break
			sleep 0.5
		done
		lo="$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 http://127.0.0.1:18140/ || true)"
		[ "$code" = 200 ] && [ "$lo" = 000 ] && pass "tcp: published on $LAN_IP answers there, and not on loopback" \
			|| fail "tcp: published on $LAN_IP: $code there, $lo on loopback"
		got="$($D run --rm alpine:3.21 wget -q -T 5 -O /dev/null "http://$LAN_IP:18140/" 2>&1 && echo ok || echo failed)"
		[ "$got" = ok ] && pass "a container reaches it at that address too" || fail "a container could not reach $LAN_IP:18140"
	else
		fail "a container published on $LAN_IP did not start: $(tail -1 "$LIGHTER_HOME/onmac.err")"
	fi
	# A Mac service on the same address, not published by Docker: still the Mac's.
	MAC_PORT_FILE="$(mktemp -t lighter-m3-macport)"
	python3 -c '
import http.server, sys
server = http.server.HTTPServer((sys.argv[2], 0), http.server.SimpleHTTPRequestHandler)
open(sys.argv[1], "w").write(str(server.server_port))
server.serve_forever()' "$MAC_PORT_FILE" "$LAN_IP" >/dev/null 2>&1 &
	MACSRV=$!
	for _ in $(seq 1 50); do [ -s "$MAC_PORT_FILE" ] && break; sleep 0.1; done
	got="$($D run --rm alpine:3.21 wget -q -T 5 -O /dev/null "http://$LAN_IP:$(cat "$MAC_PORT_FILE")/" 2>&1 && echo ok || echo failed)"
	kill "$MACSRV" 2>/dev/null; rm -f "$MAC_PORT_FILE"
	[ "$got" = ok ] && pass "a container still reaches a Mac service on that address" || fail "a container could not reach the Mac's own service on $LAN_IP"
	if $D run -d --name m3-onmac-udp -p "$LAN_IP:18141:9/udp" alpine/socat:1.8.0.0 -T 5 UDP-RECVFROM:9,fork EXEC:'/bin/echo udp-onmac' >/dev/null 2>&1; then
		reply="$(python3 -c '
import socket, sys
for _ in range(20):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(0.5)
    s.sendto(b"x", (sys.argv[1], 18141))
    try:
        print(s.recvfrom(64)[0].decode().strip()); break
    except OSError:
        pass' "$LAN_IP")"
		[ "$reply" = udp-onmac ] && pass "udp: published on $LAN_IP answers there" || fail "udp: published on $LAN_IP answered: ${reply:-nothing}"
	else
		fail "a UDP container published on $LAN_IP did not start"
	fi
	$D rm -f m3-onmac m3-onmac-udp >/dev/null 2>&1
fi
if [ -n "$V6_IP" ]; then
	$D rm -f m3-onmac6 >/dev/null 2>&1
	if $D run -d --name m3-onmac6 -p "[$V6_IP]:18142:8000" python:3.12-slim python3 -m http.server --bind :: 8000 >/dev/null 2>"$LIGHTER_HOME/onmac6.err"; then
		code=000
		for _ in $(seq 1 20); do
			code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "http://[$V6_IP]:18142/" || true)"
			[ "$code" = 200 ] && break
			sleep 0.5
		done
		[ "$code" = 200 ] && pass "v6: published on $V6_IP answers there" || fail "v6: published on $V6_IP: $code"
	else
		fail "a container published on $V6_IP did not start: $(tail -1 "$LIGHTER_HOME/onmac6.err")"
	fi
	$D rm -f m3-onmac6 >/dev/null 2>&1
fi

echo "==> A caller on an IPv6 link-local address"
# fe80:: is on-link everywhere, so a container could not answer such a caller
# as itself; it is presented as the guest, as the Mac itself is. Over UDP it
# was not presented at all, and got no answer (#66). The Mac's own link-local
# address takes the same path as a neighbour's.
LL_IP="$([ -n "$LAN_IF" ] && ifconfig "$LAN_IF" 2>/dev/null | awk '/inet6 fe80/ {print $2; exit}' | cut -d% -f1)"
if [ -n "$LL_IP" ]; then
	$D rm -f m3-ll >/dev/null 2>&1
	$D run -d --name m3-ll -p 18160:9/udp alpine/socat:1.8.0.0 -T 5 UDP6-RECVFROM:9,fork EXEC:'/bin/echo udp-ll' >/dev/null 2>&1
	reply="$(python3 -c '
import socket, sys
info = socket.getaddrinfo(sys.argv[1], 18160, type=socket.SOCK_DGRAM)[0]
for _ in range(20):
    s = socket.socket(info[0], socket.SOCK_DGRAM); s.settimeout(0.5); s.sendto(b"x", info[4])
    try:
        print(s.recvfrom(64)[0].decode().strip()); break
    except OSError:
        pass' "$LL_IP%$LAN_IF")"
	[ "$reply" = udp-ll ] && pass "udp: a link-local caller ($LL_IP) is answered" || fail "udp: a link-local caller got: ${reply:-nothing}"
	$D rm -f m3-ll >/dev/null 2>&1
fi

echo "==> HTTP immediately after connection bursts"
if python3 scripts/test-publish-burst.py --docker-host "unix://$LIGHTER_HOME/docker.sock"; then
	pass "connection bursts preserve HTTP on all-interface and loopback publishes"
else
	fail "published connection burst; evidence is retained under .logs/lighter-burst-*"
fi

echo
if [ "$FAILED" -eq 0 ]; then
	if [ "$SKIPPED" -gt 0 ]; then echo "m3-publish: all checks passed ($SKIPPED skipped)"; else echo "m3-publish: all checks passed"; fi
else
	echo "m3-publish: FAILED"
fi
exit $FAILED
