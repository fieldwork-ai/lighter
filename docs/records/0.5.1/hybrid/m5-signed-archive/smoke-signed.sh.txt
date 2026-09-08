#!/usr/bin/env bash
# Fresh-install smoke of a release tarball under a private LIGHTER_HOME:
# doctor, start, an arm64 and an amd64 container, a published TCP and UDP
# port on the LAN address, stop. Nothing of the daily driver is touched.
set -u
unset LIGHTER_GUEST_DIR LIGHTER_CMDLINE_EXTRA LIGHTER_BACKGROUND_RAM DOCKER_HOST DOCKER_CONTEXT DOCKER_CONFIG
OUT="${2:?evidence output directory}"
mkdir "$OUT"
OUT="$(cd "$OUT" && pwd)"
TARBALL="${1:?tarball}"
ROOT="$(mktemp -d -t lighter-smoke)"
export LIGHTER_HOME="$ROOT/home"
mkdir -p "$LIGHTER_HOME"
export DOCKER_CONFIG="$ROOT/docker-config"
printf '%s\n' '{"cpus":4,"memory_mib":4096,"disk_gib":64,"shares":[],"publish":"lan"}' > "$LIGHTER_HOME/config.json"
python3 - <<'PORTS'
import socket
for port,kind in [(18098,socket.SOCK_STREAM),(18094,socket.SOCK_DGRAM)]:
 with socket.socket(socket.AF_INET,kind) as s:s.bind(("0.0.0.0",port))
PORTS
[ "$?" -eq 0 ] || exit 1
tar -xzf "$TARBALL" -C "$ROOT"
DIR="$(ls -d "$ROOT"/lighter-*/ | head -1)"
L="$DIR/bin/lighter"
export LIGHTER_GUEST_DIR="$DIR/share/lighter"
D="docker -H unix://$LIGHTER_HOME/docker.sock"
ok() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
bad() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
FAILED=0
cleanup() { "$L" stop >/dev/null 2>&1 || true; cp "$ROOT"/*.log "$OUT/" 2>/dev/null || true; cp "$LIGHTER_HOME/machine.log" "$OUT/" 2>/dev/null || true; python3 scripts/records/unregister-test-bundles.py "$ROOT" >"$OUT/unregister.log" 2>&1 || true; rm -rf "$ROOT"; }
trap cleanup EXIT
echo "==> $("$L" --version) from $TARBALL"
"$L" doctor >"$ROOT/doctor.log" 2>&1 && ok "doctor" || { bad "doctor: $(tail -3 "$ROOT/doctor.log" | tr '\n' ' ')"; }
"$L" start --timeout 120 >"$ROOT/start.log" 2>&1 && ok "start: $(grep -m1 Docker "$ROOT/start.log")" || { bad "start: $(tail -2 "$ROOT/start.log" | tr '\n' ' ')"; exit 1; }
out="$($D run --rm alpine:3.21 uname -m 2>/dev/null)"; [ "$out" = aarch64 ] && ok "arm64 container: $out" || bad "arm64: $out"
out="$($D run --rm --platform linux/amd64 alpine:3.21 uname -m 2>/dev/null)"; [ "$out" = x86_64 ] && ok "amd64 container: $out" || bad "amd64: $out"
LAN_IP="$(ipconfig getifaddr en0 2>/dev/null || echo 127.0.0.1)"
$D run -d --rm --name smoke-http -p 18098:80 alpine:3.21 sh -c 'while true; do printf "HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok" | nc -l -p 80; done' >/dev/null 2>&1; sleep 3
code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://$LAN_IP:18098/")"; [ "$code" = 200 ] && ok "published TCP on $LAN_IP" || bad "TCP publish: $code"
$D run -d --rm --name smoke-udp -p 18094:9/udp alpine/socat:1.8.0.0 UDP6-RECVFROM:9,ipv6only=0,fork EXEC:cat >/dev/null 2>&1; sleep 3
r="$(python3 -c 'import socket,sys; s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.settimeout(5); s.sendto(b"hi",(sys.argv[1],18094)); print(s.recvfrom(64)[0].decode())' "$LAN_IP" 2>&1)"; [ "$r" = hi ] && ok "published UDP on $LAN_IP" || bad "UDP publish: $r"
$D run --rm alpine:3.21 sh -c 'ip -6 route show default | grep -q . && echo v6' 2>/dev/null | grep -q v6 && ok "container has a v6 route" || bad "no v6 route"
$D rm -f smoke-http smoke-udp >/dev/null 2>&1
"$L" status | head -2
"$L" stop >/dev/null 2>&1 && ok "stop" || bad "stop"
[ "$FAILED" -eq 0 ] && echo "smoke: all passed" || echo "smoke: FAILED"
printf '%s\n' "$FAILED" > "$OUT/exit-code"
exit $FAILED
