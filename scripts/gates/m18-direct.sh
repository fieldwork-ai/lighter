#!/usr/bin/env bash
# Milestone 18 gate: the Mac reaches containers directly (0.13).
#
# The link, a network of the Mac and the machine alone, puts Docker's
# networks where the Mac can route to them. What that has to mean:
#
#   1. lighter status says so, naming the subnet
#   2. the Mac reaches a container's unpublished port at the container's
#      own address, and the container sees the Mac's address on the link
#   3. a network made later is inside the subnet, and reachable the same way
#   4. names: `<container>.lighter.local`, `<service>.<project>.lighter.local`
#      for Compose, and a host-network container at the machine's address,
#      each resolved in well under a second (the IPv6 question answered
#      too, so getaddrinfo does not wait out mDNS's five seconds)
#   5. a removed container's name stops resolving at once
#   6. containers resolve the names too, through the Mac's resolver
#   7. IPv6 the same, for the default bridge and a network made with IPv6;
#      names a label chooses, and names under a container's own
#   8. an address in the subnet that nothing has fails, and does not loop
#
# Needs com.apple.vm.networking: a release build (LIGHTER_BIN), or a
# package made with `scripts/package-release.sh <version> --skip-notarize`.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

PROFILE="${PROFILE:-release}"
LIGHTER="${LIGHTER_BIN:-target/$PROFILE/lighter}"
FAILED=0
pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }

entitled() {
	codesign -d --entitlements - --xml "$(dirname "$LIGHTER")/../share/lighter/lighter.app" 2>/dev/null \
		| grep -q com.apple.vm.networking
}
if ! entitled; then
	echo "m18: skipped, this lighter does not hold com.apple.vm.networking (run a release build with LIGHTER_BIN)"
	exit 0
fi

export LIGHTER_HOME="$(mktemp -d -t lighter-m18-home)"
export DOCKER_HOST="unix://$LIGHTER_HOME/docker.sock"
cleanup() {
	docker rm -f m18-web m18-db-1 m18-db-2 m18-host m18-other m18-v6 m18-v6b m18-named >/dev/null 2>&1 || true
	docker network rm m18-net m18-net6 >/dev/null 2>&1 || true
	"$LIGHTER" stop >/dev/null 2>&1 || true
	[ "$FAILED" = 0 ] || { mkdir -p "$ROOT/.logs"; cp "$LIGHTER_HOME/machine.log" "$ROOT/.logs/m18-machine.log" 2>/dev/null; } || true
	rm -rf "$LIGHTER_HOME"
}
trap cleanup EXIT

# /usr/bin/curl, not a Homebrew one: macOS's Local Network privacy refuses
# an app it has not asked about, and Apple's own tools are exempt.
CURL=/usr/bin/curl
get() { "$CURL" -s -o /dev/null -w '%{http_code}' --max-time "${2:-3}" "$1" || true; }
# Resolves a name as any Mac program does (getaddrinfo, both families),
# printing the first IPv4 address and the seconds it took.
resolve() {
	/usr/bin/python3 - "$1" <<'PY'
import socket, sys, time
start = time.time()
try:
    infos = socket.getaddrinfo(sys.argv[1], 80, 0, socket.SOCK_STREAM)
    v4 = [i[4][0] for i in infos if i[0] == socket.AF_INET]
    print(v4[0] if v4 else "none", round(time.time() - start, 2))
except socket.gaierror:
    print("none", round(time.time() - start, 2))
PY
}
ip_of() { docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}' "$1" | awk '{print $1}'; }

echo "==> A machine with the link"
"$LIGHTER" config --direct on >/dev/null
"$LIGHTER" start >/dev/null 2>&1 || { fail "lighter start failed"; "$LIGHTER" logs | tail -15; exit 1; }
docker pull -q python:3.12-slim >/dev/null 2>&1
docker pull -q alpine:3.21 >/dev/null 2>&1
line="$("$LIGHTER" status 2>/dev/null | grep '^  direct ' || true)"
SUBNET="$(grep -oE '[0-9]+\.[0-9]+\.0\.0/16' <<<"$line" | head -1)"
if [ -n "$SUBNET" ]; then
	pass "lighter status: ${line#  direct     }"
else
	fail "lighter status has no subnet for direct access: ${line:-nothing}"
	exit 1
fi
AB="${SUBNET%.0.0/16}"
in_subnet() { case "$1" in "$AB".*) return 0 ;; *) return 1 ;; esac; }

echo
echo "==> The Mac reaches a container at its own address"
docker run -d --name m18-web python:3.12-slim python3 -m http.server 80 >/dev/null
WEB="$(ip_of m18-web)"
in_subnet "$WEB" && pass "the default bridge is inside the subnet ($WEB)" || fail "the container's address $WEB is outside $SUBNET"
code=000
for _ in $(seq 1 30); do
	code="$(get "http://$WEB/" 2)"
	[ "$code" = 200 ] && break
	sleep 0.5
done
[ "$code" = 200 ] && pass "an unpublished port answers the Mac at $WEB" || fail "$WEB:80 did not answer the Mac ($code)"
seen="$(docker logs m18-web 2>&1 | grep -oE '^[0-9.]+' | tail -1)"
[ "$seen" = "$AB.0.1" ] && pass "the container sees the Mac's own address on the link ($seen)" \
	|| fail "the container saw the Mac as $seen, not $AB.0.1"

echo
echo "==> A network made later"
docker network create m18-net >/dev/null
docker run -d --name m18-db-1 --network m18-net \
	--label com.docker.compose.project=m18shop --label com.docker.compose.service=db \
	python:3.12-slim python3 -m http.server 8000 >/dev/null
docker run -d --name m18-db-2 --network m18-net \
	--label com.docker.compose.project=m18shop --label com.docker.compose.service=db \
	python:3.12-slim python3 -m http.server 8000 >/dev/null
DB1="$(ip_of m18-db-1)"
in_subnet "$DB1" && pass "its subnet is inside $SUBNET ($(docker network inspect -f '{{range .IPAM.Config}}{{.Subnet}}{{end}}' m18-net))" \
	|| fail "the new network's container is at $DB1, outside $SUBNET"
code=000
for _ in $(seq 1 30); do
	code="$(get "http://$DB1:8000/" 2)"
	[ "$code" = 200 ] && break
	sleep 0.5
done
[ "$code" = 200 ] && pass "and reachable from the Mac" || fail "$DB1:8000 did not answer the Mac ($code)"

echo
echo "==> Names"
docker run -d --name m18-host --network host python:3.12-slim python3 -m http.server 18181 >/dev/null
sleep 2
# A name and the addresses it may resolve to.
check_name() {
	local name="$1" got took
	shift
	read -r got took <<<"$(resolve "$name")"
	if [[ " $* " == *" $got "* ]] && awk "BEGIN{exit !($took < 1)}"; then
		pass "$name is $got (${took}s)"
	else
		fail "$name resolved to $got in ${took}s; wanted one of $* in under a second"
	fi
}
check_name m18-web.lighter.local "$WEB"
check_name db.m18shop.lighter.local "$DB1" "$(ip_of m18-db-2)"
check_name m18-db-2.lighter.local "$(ip_of m18-db-2)"
check_name m18-host.lighter.local "$AB.0.2"
code="$(get "http://m18-web.lighter.local/" 3)"
[ "$code" = 200 ] && pass "curl http://m18-web.lighter.local/ answers" || fail "curl by name failed ($code)"
code="$(get "http://m18-host.lighter.local:18181/" 3)"
[ "$code" = 200 ] && pass "and a host-network container by its name" || fail "the host-network container by name failed ($code)"
got="$(docker run --rm alpine:3.21 sh -c 'nslookup m18-web.lighter.local 2>/dev/null | grep -A1 "^Name" | grep -oE "[0-9]+(\.[0-9]+){3}"' | head -1)"
[ "$got" = "$WEB" ] && pass "a container resolves the names too ($got)" || fail "a container resolved m18-web.lighter.local to '${got}'"

# IPv6: a container on the default bridge has an address on the link's
# /64, and one on a network made with IPv6 does too; both answer the Mac
# by it and by name, in both families at once.
docker run -d --name m18-v6 python:3.12-slim python3 -m http.server --bind :: 8000 >/dev/null
docker network create --ipv6 m18-net6 >/dev/null
docker run -d --name m18-v6b --network m18-net6 python:3.12-slim python3 -m http.server --bind :: 8000 >/dev/null
for c in m18-v6 m18-v6b; do
	v6="$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.GlobalIPv6Address}}{{end}}' "$c")"
	code=000
	for _ in $(seq 1 30); do
		code="$(get "http://[$v6]:8000/" 2)"
		[ "$code" = 200 ] && break
		sleep 0.5
	done
	[ "$code" = 200 ] && pass "$c answers the Mac over IPv6 at $v6" || fail "$c at [$v6]:8000 did not answer the Mac ($code)"
	both="$(/usr/bin/python3 -c "
import socket
print(len({i[0] for i in socket.getaddrinfo('$c.lighter.local', 80, 0, socket.SOCK_STREAM)}))")"
	[ "$both" = 2 ] && pass "$c.lighter.local has both families" || fail "$c.lighter.local resolved to ${both:-no} families"
done
got="$("$CURL" -s -6 -m 5 -o /dev/null -w '%{http_code}' http://m18-v6.lighter.local:8000/ || true)"
[ "$got" = 200 ] && pass "curl -6 http://m18-v6.lighter.local:8000/ answers" || fail "curl -6 by name: $got"
docker rm -f m18-v6 m18-v6b >/dev/null 2>&1; docker network rm m18-net6 >/dev/null 2>&1

# Names a container's owner chooses (`lighter.domains`, any .local name,
# `*.` for all under one), and any name under a container's own.
docker run -d --name m18-named --label "lighter.domains=m18-app.local,*.m18-wild.local" \
	python:3.12-slim python3 -m http.server 80 >/dev/null
NAMED="$(ip_of m18-named)"
sleep 2
check_name m18-app.local "$NAMED"
check_name api.v2.m18-wild.local "$NAMED"
check_name admin.m18-named.lighter.local "$NAMED"
docker rm -f m18-named >/dev/null 2>&1

# A goodbye clears the Mac's cache at once; without one the name would
# outlive its container by its TTL.
docker rm -f m18-web >/dev/null
gone=""
for i in $(seq 1 12); do
	read -r got _ <<<"$(resolve m18-web.lighter.local)"
	[ "$got" = none ] && { gone="$((i * 250))"; break; }
	sleep 0.25
done
[ -n "$gone" ] && pass "a removed container's name stops resolving (within ${gone} ms)" \
	|| fail "m18-web.lighter.local still resolves to $got three seconds after its container went"

echo
echo "==> An address nothing has"
code="$(get "http://$AB.200.7/" 4)"
[ "$code" = 000 ] && pass "$AB.200.7 does not answer" || fail "$AB.200.7 answered ($code)"
if docker run --rm alpine:3.21 wget -q -T 3 -O /dev/null "http://$AB.200.7/" >/dev/null 2>&1; then
	fail "a container reached $AB.200.7"
else
	pass "nor from a container, which is refused rather than sent back to the Mac"
fi

if [ "$FAILED" = 0 ]; then
	echo
	echo "m18: the Mac reaches containers directly"
fi
exit "$FAILED"
