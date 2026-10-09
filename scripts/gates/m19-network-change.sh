#!/usr/bin/env bash
# Milestone 19 gate: the Mac's network changes under a running machine.
#
# Wi-Fi goes off and comes back while containers run. What has to hold:
#
#   1. while it is off, what needs no network still works: a published port
#      on localhost, the Mac reaching a container directly and by name
#   2. once it is back, within a minute and without a restart: a
#      container's DNS, HTTPS, ping and traceroute's first hops, and the
#      published port from the Mac's network address
#   3. with LAN mode on, the machine has its address on the network again
#
# It turns the Mac's Wi-Fi off, so it runs only when asked
# (LIGHTER_GATE_NETWORK_CHANGE=1), on a Mac whose session survives that:
# run it detached (nohup) when connected over the same Wi-Fi.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

PROFILE="${PROFILE:-release}"
LIGHTER="${LIGHTER_BIN:-target/$PROFILE/lighter}"
FAILED=0
pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
note() { printf '  \033[33m··\033[0m   %s\n' "$*"; }

if [ "${LIGHTER_GATE_NETWORK_CHANGE:-}" != 1 ]; then
	echo "m19: skipped, it turns the Mac's Wi-Fi off (set LIGHTER_GATE_NETWORK_CHANGE=1)"
	exit 0
fi
WIFI="$(networksetup -listallhardwareports | awk '/Hardware Port: Wi-Fi/ {getline; print $2}')"
[ -n "$WIFI" ] || { echo "m19: skipped, this Mac has no Wi-Fi"; exit 0; }

export LIGHTER_HOME="$(mktemp -d -t lighter-m19-home)"
export DOCKER_HOST="unix://$LIGHTER_HOME/docker.sock"
CURL=/usr/bin/curl
cleanup() {
	networksetup -setairportpower "$WIFI" on >/dev/null 2>&1
	docker rm -f m19-web >/dev/null 2>&1 || true
	"$LIGHTER" stop >/dev/null 2>&1 || true
	[ "$FAILED" = 0 ] || { mkdir -p "$ROOT/.logs"; cp "$LIGHTER_HOME/machine.log" "$ROOT/.logs/m19-machine.log" 2>/dev/null; } || true
	rm -rf "$LIGHTER_HOME"
}
trap cleanup EXIT

# Seconds until `check` (a command) succeeds, at most `limit`; empty if never.
within() {
	local limit="$1" start=$SECONDS
	shift
	while [ $((SECONDS - start)) -lt "$limit" ]; do
		"$@" >/dev/null 2>&1 && { echo $((SECONDS - start)); return 0; }
		sleep 1
	done
	return 1
}
online() { route -n get default >/dev/null 2>&1 && $CURL -s -m 3 -o /dev/null https://www.apple.com/; }

echo "==> A machine with containers"
[ "${LIGHTER_GATE_LAN:-}" = 1 ] && "$LIGHTER" config --lan on >/dev/null
"$LIGHTER" start >/dev/null 2>&1 || { fail "lighter start failed"; exit 1; }
docker pull -q alpine:3.21 >/dev/null 2>&1
docker pull -q python:3.12-slim >/dev/null 2>&1
docker run -d --name m19-web -p 18191:8000 python:3.12-slim python3 -m http.server 8000 >/dev/null
WEB="$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' m19-web)"
direct="$("$LIGHTER" status 2>/dev/null | grep -c '^  direct     containers')"
within 30 $CURL -s -m 2 -o /dev/null http://127.0.0.1:18191/ >/dev/null || { fail "the published port never answered"; exit 1; }
outbound() { docker run --rm alpine:3.21 wget -q -T 5 -O /dev/null https://www.apple.com/; }
outbound && pass "a container reaches the internet" || fail "no internet from a container before the change"

echo
echo "==> Wi-Fi off"
networksetup -setairportpower "$WIFI" off
sleep 5
[ "$($CURL -s -m 3 -o /dev/null -w '%{http_code}' http://127.0.0.1:18191/)" = 200 ] \
	&& pass "the published port still answers on localhost" || fail "the published port stopped answering on localhost"
if [ "$direct" = 1 ]; then
	[ "$($CURL -s -m 3 -o /dev/null -w '%{http_code}' "http://$WEB:8000/")" = 200 ] \
		&& pass "the Mac still reaches the container at $WEB" || fail "the container at $WEB stopped answering the Mac"
	[ "$($CURL -s -m 3 -o /dev/null -w '%{http_code}' http://m19-web.lighter.local:8000/)" = 200 ] \
		&& pass "and as m19-web.lighter.local" || fail "m19-web.lighter.local stopped answering"
fi

echo
echo "==> Wi-Fi back"
networksetup -setairportpower "$WIFI" on
if t="$(within 90 online)"; then
	note "the Mac was online again after ${t}s"
else
	fail "the Mac itself did not come back online in 90 s; nothing more to judge"
	exit 1
fi
t="$(within 60 outbound)" && pass "a container reaches the internet again (${t}s after the Mac)" || fail "a container could not reach the internet within 60 s of the Mac"
t="$(within 30 docker run --rm alpine:3.21 nslookup apple.com)" && pass "DNS ($t s)" || fail "a container's DNS did not come back"
t="$(within 30 docker run --rm alpine:3.21 ping -c 1 -W 3 1.1.1.1)" && pass "ping ($t s)" || fail "a container's ping did not come back"
hops="$(docker run --rm alpine:3.21 traceroute -n -m 3 -w 2 -q 1 1.1.1.1 2>/dev/null | awk 'NR>1 {print $2}' | tr '\n' ' ')"
case "$hops" in *192.168.127.1*) pass "traceroute ($hops)" ;; *) fail "traceroute after the change: ${hops:-nothing}" ;; esac
MAC_LAN="$(ipconfig getifaddr "$WIFI" 2>/dev/null)"
t="$(within 30 $CURL -s -m 3 -o /dev/null "http://$MAC_LAN:18191/")" && pass "the published port answers on the Mac's network address, $MAC_LAN ($t s)" \
	|| fail "the published port does not answer on $MAC_LAN"
if [ "${LIGHTER_GATE_LAN:-}" = 1 ]; then
	lan_back() { "$LIGHTER" status 2>/dev/null | grep -qE '^  lan +[0-9]'; }
	t="$(within 60 lan_back)" && pass "LAN mode has its address again ($t s): $("$LIGHTER" status | grep '^  lan')" \
		|| fail "LAN mode has no address a minute after Wi-Fi came back: $("$LIGHTER" status | grep '^  lan')"
fi

if [ "$FAILED" = 0 ]; then
	echo
	echo "m19: a network change needs no restart"
fi
exit "$FAILED"
