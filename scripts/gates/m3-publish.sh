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

LAN_IP="$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || true)"
# The Mac's stable global v6 address, not a temporary one (those rotate).
V6_IP="$(ifconfig en0 2>/dev/null | awk '/inet6/ && /autoconf/ && /secured/ && !/temporary/ {print $2; exit}')"
[ -n "$LAN_IP" ] || skip "no LAN address on en0/en1; LAN checks skipped"
[ -n "$V6_IP" ] || skip "no global IPv6 address on en0; global v6 checks skipped"

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
