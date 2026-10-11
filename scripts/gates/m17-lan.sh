#!/usr/bin/env bash
# Milestone 17 gate: the machine on the Mac's network (LAN mode, 0.12).
#
# With `lighter config --lan on`, a second card bridged to one of the Mac's
# puts the guest on the user's network. What that has to mean:
#
#   1. it gets an address there, by DHCP (or LIGHTER_GATE_LAN_ADDRESS)
#   2. multicast leaves by it, so Home Assistant's choice of default
#      adapter, the source address towards 224.0.0.251, is that address
#   3. a host-network container discovers what is on the network (mDNS: the
#      Mac itself answers, so at least one host does)
#   4. it is reachable there, and only where a container listens: the
#      agent's own ports are not, and a published port is as the publish
#      scope says
#   5. both kinds of container still reach the network: a host-network one
#      directly, a bridged one through the streams, as before
#
# The Mac is the LAN peer: its traffic to the guest's address crosses the
# bridge and meets the guest's firewall as any device's would.
#
# Needs the network helper: installed (`sudo lighter lan enable`), or run by
# hand with LIGHTER_BRIDGE_SOCKET naming its socket:
#   sudo target/release/lighter-bridge --socket /tmp/lb.sock --allow-uid $(id -u) --any-client
# Without either the gate skips, saying so.
set -u

if ! command -v cargo >/dev/null 2>&1; then
	# shellcheck disable=SC1091
	. "$HOME/.cargo/env"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

PROFILE="${PROFILE:-release}"
LIGHTER="${LIGHTER_BIN:-target/$PROFILE/lighter}"
FAILED=0
pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
note() { printf '  \033[33m··\033[0m   %s\n' "$*"; }

# A release build's app holds com.apple.vm.networking and bridges by itself;
# a source build needs the helper.
entitled() {
	codesign -d --entitlements - --xml "$(dirname "$LIGHTER")/../share/lighter/lighter.app" 2>/dev/null \
		| grep -q com.apple.vm.networking
}
if [ -z "${LIGHTER_BRIDGE_SOCKET:-}" ] && [ ! -S /var/run/dev.lighter.bridge.sock ] && ! entitled; then
	echo "m17: skipped, no network helper and no entitlement (install the helper with sudo lighter lan enable, set LIGHTER_BRIDGE_SOCKET, or run a release build with LIGHTER_BIN)"
	exit 0
fi

export LIGHTER_HOME="$(mktemp -d -t lighter-m17-home)"
export DOCKER_HOST="unix://$LIGHTER_HOME/docker.sock"
MAC_SERVER=""
cleanup() {
	[ -n "$MAC_SERVER" ] && { kill "$MAC_SERVER" 2>/dev/null; wait "$MAC_SERVER" 2>/dev/null; }
	docker rm -f m17-web m17-pub >/dev/null 2>&1 || true
	"$LIGHTER" stop >/dev/null 2>&1 || true
	[ "$FAILED" = 0 ] || cp "$LIGHTER_HOME/machine.log" "$ROOT/.logs/m17-machine.log" 2>/dev/null || true
	rm -rf "$LIGHTER_HOME"
}
trap cleanup EXIT

if [ -z "${LIGHTER_BIN:-}" ]; then
	echo "==> Building and signing the CLI"
	cargo build $([ "$PROFILE" = release ] && echo --release) -p lighter-cli >/dev/null 2>&1 || { echo "build failed"; exit 1; }
	./scripts/sign.sh "$LIGHTER" >/dev/null
fi

echo "==> A machine on the network"
"$LIGHTER" config --lan on >/dev/null
[ -n "${LIGHTER_GATE_LAN_ADDRESS:-}" ] && "$LIGHTER" config --lan-address "$LIGHTER_GATE_LAN_ADDRESS" >/dev/null
"$LIGHTER" start >/dev/null 2>&1 || { fail "lighter start failed"; "$LIGHTER" logs | tail -15; exit 1; }
docker pull -q alpine:3.21 >/dev/null 2>&1
docker pull -q python:3.12-slim >/dev/null 2>&1
state=""
for _ in $(seq 1 60); do
	state="$("$LIGHTER" status 2>/dev/null | grep '^  lan ' || true)"
	case "$state" in *waiting*|"") sleep 1 ;; *) break ;; esac
done
GUEST="$(grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' <<<"$state" | head -1)"
if [ -n "$GUEST" ] && ! grep -q "not on the network\|no address" <<<"$state"; then
	pass "the machine has an address on the network: ${state#  lan        }"
	if entitled && [ -z "${LIGHTER_BRIDGE_SOCKET:-}" ]; then
		sed 's/\x1b\[[0-9;]*m//g' "$LIGHTER_HOME/machine.log" | grep -aq 'how="in process"' \
			&& pass "lighter bridged it by itself, with no helper (com.apple.vm.networking)" \
			|| fail "an entitled build did not bridge in process: $(grep -a 'LAN card' "$LIGHTER_HOME/machine.log" | tail -1)"
	fi
else
	fail "no address on the network: ${state:-lighter status says nothing about it}"
	"$LIGHTER" doctor 2>&1 | grep -A2 LAN | sed 's/^/    /'
	exit 1
fi
"$LIGHTER" doctor 2>&1 | grep -q "ok .*LAN .*on the network as" && pass "lighter doctor says so" \
	|| fail "lighter doctor does not report the LAN card as on the network"

echo
echo "==> Multicast and discovery leave by the LAN card"
source="$(docker run --rm --network host python:3.12-slim python3 -c '
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.connect(("224.0.0.251", 1))
print(s.getsockname()[0])' 2>&1 | tail -1)"
[ "$source" = "$GUEST" ] && pass "the source towards 224.0.0.251 is the LAN address, so Home Assistant's default adapter is too" \
	|| fail "the source towards 224.0.0.251 is $source, not $GUEST"
answered="$(docker run --rm --network host python:3.12-slim python3 -c '
import socket, struct, time
q = struct.pack("!HHHHHH", 0, 0, 1, 0, 0, 0)
for label in b"_services._dns-sd._udp.local".split(b"."):
    q += bytes([len(label)]) + label
q += b"\0" + struct.pack("!HH", 12, 1)
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(0.3)
s.sendto(q, ("224.0.0.251", 5353))
seen, end = set(), time.time() + 4
while time.time() < end:
    try: seen.add(s.recvfrom(9000)[1][0])
    except socket.timeout: pass
print(len(seen))' 2>&1 | tail -1)"
[ "${answered:-0}" -ge 1 ] 2>/dev/null && pass "a host-network container's mDNS query is answered by $answered hosts on the network" \
	|| fail "no host on the network answered a host-network container's mDNS query (${answered:-none})"

echo
echo "==> Reachable where a container listens, and nowhere else"
docker run -d --name m17-web --network host python:3.12-slim python3 -m http.server 18123 >/dev/null
docker run -d --name m17-pub -p 18125:8000 python:3.12-slim python3 -m http.server 8000 >/dev/null
code=000
for _ in $(seq 1 30); do
	code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "http://$GUEST:18123/" || true)"
	[ "$code" = 200 ] && break
	sleep 0.5
done
[ "$code" = 200 ] && pass "a host-network server is reachable at the machine's own address" \
	|| fail "a host-network server is not reachable at $GUEST:18123 ($code)"
code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://$GUEST:15201/" || true)"
[ "$code" = 000 ] && pass "the agent's own ports are not" || fail "the agent's port 15201 answered at $GUEST ($code)"
code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://$GUEST:18125/" || true)"
[ "$code" = 200 ] && pass "a published port is too, as the publish scope (lan) says" \
	|| fail "a published port is not reachable at $GUEST:18125 with the scope at lan ($code)"

echo
echo "==> Both kinds of container still reach the network"
MAC_LAN="$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null)"
PORT_FILE="$(mktemp -t lighter-m17-port)"
python3 - "$PORT_FILE" >/dev/null 2>&1 <<'PY' &
import http.server, sys
server = http.server.HTTPServer(("0.0.0.0", 0), http.server.SimpleHTTPRequestHandler)
open(sys.argv[1], "w").write(str(server.server_port))
server.serve_forever()
PY
MAC_SERVER=$!
for _ in $(seq 1 50); do [ -s "$PORT_FILE" ] && break; sleep 0.1; done
PORT="$(cat "$PORT_FILE")"
rm -f "$PORT_FILE"
ROUTER="$(route -n get default 2>/dev/null | awk '/gateway:/ {print $2}')"
for mode in host bridge; do
	got="$(docker run --rm --network "$mode" alpine:3.21 wget -q -T 5 -O /dev/null "http://$MAC_LAN:$PORT/" 2>&1 && echo ok || echo failed)"
	[ "$got" = ok ] && pass "a $mode-network container reaches a device on the network (the Mac, $MAC_LAN)" \
		|| fail "a $mode-network container could not reach $MAC_LAN:$PORT"
	# The router, which answers ping: a bridge container's echo leaves by
	# the LAN card, and its reply was dropped there until 0.13.
	if docker run --rm --network "$mode" alpine:3.21 ping -c 2 -W 3 "$ROUTER" >/dev/null 2>&1; then
		pass "a $mode-network container pings a device on the network (the router, $ROUTER)"
	else
		fail "a $mode-network container's ping to $ROUTER went unanswered"
	fi
done

if [ "$FAILED" = 0 ]; then
	echo
	echo "m17: the machine is on the network"
fi
exit "$FAILED"
