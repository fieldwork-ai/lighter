#!/usr/bin/env bash
# Milestone 8 gate: a day's work, and a night's sleep.
#
# Everything before this proved a capability. This one asks whether the thing
# is usable: start it the way a person does, bring up the stack they leave
# running, edit a file on the Mac and see it in a container, put the machine
# through what a closed lid does to it, and check that all of it still works
# afterwards.
#
# # About the sleep
#
# The gate cannot suspend the Mac — it would take the session with it. What a
# sleep actually does to a guest is stop its clock, so that is what is done
# here: the clock is put an hour back from inside the guest (a privileged
# container may), and then the same recovery path that `IOKit`'s wake
# notification triggers is run. It used to be skewed through the control
# channel's `time` verb, which now carries no time: the agent asks the host. Everything is exercised except IOKit's delivery of the
# event itself, which is one function call and cannot be faked convincingly.
set -euo pipefail

if ! command -v cargo >/dev/null 2>&1; then
	# shellcheck disable=SC1091
	. "$HOME/.cargo/env"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

PROFILE="${PROFILE:-release}"
LIGHTER="target/$PROFILE/lighter"
COMPOSE="scripts/gates/fixtures/daily.yml"
# Under $HOME, because that is what the machine shares by default and a bind
# mount of a path the guest cannot see produces an empty directory rather than
# an error — which nginx then serves as a 403 and nothing explains why.
SHARE="$HOME/.lighter-gate"
rm -rf "$SHARE"
mkdir -p "$SHARE"
export LIGHTER_GATE_SHARE="$SHARE"

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
note() { printf '  \033[33m··\033[0m   %s\n' "$*"; }
FAILED=0

for tool in docker curl; do
	command -v "$tool" >/dev/null 2>&1 || { echo "$tool is required" >&2; exit 1; }
done

# The gate gets its own machine in its own home. The docker context is
# global and belongs to the default home alone, so none of this touches a
# daily-driver lighter that may be running right beside it — which it once
# did, stopping the machine the person at the keyboard was using.
export LIGHTER_HOME="$(mktemp -d -t lighter-m8-home)"
export DOCKER_HOST="unix://$LIGHTER_HOME/docker.sock"
if [ -n "${LIGHTER_BENCH_OWNER_FILE:-}" ]; then
	python3 - "$LIGHTER_BENCH_OWNER_FILE" "$LIGHTER_HOME/lighter.app/Contents/MacOS/lighter" <<'PYOWNER'
import json, sys
with open(sys.argv[1], 'a') as f:
    f.write(json.dumps(sys.argv[2]) + '\n')
PYOWNER
fi

NEVER_CLOSES=""
cleanup() {
	[ -n "$NEVER_CLOSES" ] && kill "$NEVER_CLOSES" 2>/dev/null
	docker compose -f "$COMPOSE" down -v --timeout 10 >/dev/null 2>&1 || true
	"$LIGHTER" stop >/dev/null 2>&1 || true
	rm -rf "$SHARE" "$LIGHTER_HOME"
}
trap cleanup EXIT

echo "==> Building and signing the CLI"
cargo build $([ "$PROFILE" = release ] && echo --release) -p lighter-cli
./scripts/sign.sh "$LIGHTER" >/dev/null

# The stack bind-mounts this, and nginx has to find something to serve.
echo "<h1>lighter</h1>" > "$SHARE/index.html"

echo
echo "==> Starting the way a person does"
if "$LIGHTER" start >/dev/null 2>&1; then
	pass "lighter start brought up a machine reachable at DOCKER_HOST"
else
	fail "lighter start failed"
	"$LIGHTER" logs 2>/dev/null | tail -15 | sed 's/^/    /'
	exit 1
fi

if "$LIGHTER" doctor >/dev/null 2>&1; then
	pass "lighter doctor is happy"
else
	fail "lighter doctor reports a problem"
	"$LIGHTER" doctor | sed 's/^/    /'
fi

# Reachable through the context rather than an exported DOCKER_HOST, because
# that is what a person's shell looks like.
# DOCKER_HOST is how a custom home is reached; the global context belongs to
# the default home and this gate must never touch it.
if docker version >/dev/null 2>&1; then
	pass "the docker CLI reaches the gate's machine through DOCKER_HOST"
else
	fail "DOCKER_HOST does not answer"
fi

echo
echo "==> Bringing up a day's stack"
started="$(date +%s)"
# Retried once. Bringing a stack up immediately after tearing one down
# occasionally fails with "network … not found" — the daemon has removed the
# network and a container is started against the id before the replacement is
# registered. It is a race in compose and the daemon, not in the machine under
# them, and it does not survive a second attempt.
compose_up() {
	docker compose -f "$COMPOSE" up -d --wait >/tmp/lighter-m8-compose.log 2>&1
}
if compose_up || { sleep 3; docker compose -f "$COMPOSE" down --timeout 5 >/dev/null 2>&1; compose_up; }; then
	pass "six services healthy in $(( $(date +%s) - started ))s"
else
	fail "compose up did not go green"
	tail -5 /tmp/lighter-m8-compose.log | sed 's/^/    /'
	docker compose -f "$COMPOSE" ps 2>&1 | sed 's/^/    /'
	docker compose -f "$COMPOSE" logs --tail 20 2>&1 | tail -30 | sed 's/^/    /'
fi

check_port() {
	local name="$1" url="$2"
	code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 "$url" 2>/dev/null || true)"
	if [ "$code" != "000" ] && [ -n "$code" ]; then
		pass "$name answers on localhost (HTTP $code)"
	else
		fail "$name is not reachable at $url"
	fi
}
check_port "mailpit" "http://127.0.0.1:18025/"
check_port "minio" "http://127.0.0.1:19000/minio/health/live"
check_port "nginx" "http://127.0.0.1:18080/"

# Deliberately WITH an open stdin that never closes. This exact shape hung
# for an hour once, and the /dev/null redirect that "fixed" it was masking
# a real bug: the guest agent only tore the docker relay down when BOTH
# directions ended, so an exec whose stdin never EOFs never saw the reply
# to a command that finished in milliseconds. The agent half-closes now,
# and this check holds it there — a hang here is that regression, and the
# sleep-pipe plus timeout turns it into a failure instead of an hour.
EXEC_FIFO="$(mktemp -u -t lighter-exec-fifo)"
mkfifo "$EXEC_FIFO"
# Open read-write so the fifo never delivers EOF: exactly the stdin a
# terminal presents.
exec 8<>"$EXEC_FIFO"
docker compose -f "$COMPOSE" exec -T postgres pg_isready -U postgres <&8 >/dev/null 2>&1 &
EXEC_PID=$!
EXEC_OK=0
for _ in $(seq 1 30); do
	kill -0 "$EXEC_PID" 2>/dev/null || { EXEC_OK=1; break; }
	sleep 1
done
exec 8<&-
rm -f "$EXEC_FIFO"
if [ "$EXEC_OK" = 1 ] && wait "$EXEC_PID"; then
	pass "postgres is serving, and exec returns with stdin held open"
else
	kill "$EXEC_PID" 2>/dev/null
	fail "exec did not return with stdin held open (the agent's half-close regressed)"
fi

echo
echo "==> A file edited on the Mac, seen by a container"
echo "<h1>edited</h1>" > "$SHARE/index.html"
served=""
for _ in $(seq 1 20); do
	served="$(curl -s --max-time 5 http://127.0.0.1:18080/ 2>/dev/null || true)"
	case "$served" in *edited*) break ;; esac
	sleep 0.5
done
case "$served" in
*edited*) pass "the change reached the container" ;;
*) fail "the container still serves: ${served:-nothing}" ;;
esac

echo
echo "==> Connections a container closed, to a server that never closes its end"
# Home Assistant lost every outbound connection after a night (#57): each
# poll of a device that keeps idle connections open left the outbound proxy
# holding four descriptors for good once the container closed its end,
# until it had none. The proxy probes a container's side once it is quiet,
# and lets a stream go when that side is gone.
PORT_FILE="$(mktemp -t lighter-m8-port)"
python3 - "$PORT_FILE" <<'PY' &
import socket, sys
s = socket.socket()
s.bind(("0.0.0.0", 0))
s.listen(512)
open(sys.argv[1], "w").write(str(s.getsockname()[1]))
held = []
while True:
    held.append(s.accept()[0])
PY
NEVER_CLOSES=$!
for _ in $(seq 1 50); do [ -s "$PORT_FILE" ] && break; sleep 0.1; done
NC_PORT="$(cat "$PORT_FILE")"
rm -f "$PORT_FILE"
proxy_fds() {
	docker run --rm --pid=host --privileged alpine:3.21 sh -c \
		'for p in $(pidof lighter-agent); do if grep -q tcp-proxy /proc/$p/cmdline; then ls /proc/$p/fd | wc -l; fi; done' 2>/dev/null \
		|| echo 0
}
# The other half of the rule, alongside: a container that only shuts down
# its writes keeps its stream, past the probes, for a reply that comes
# late. The server reads to end of file and answers 75 s later.
LATE_FILE="$(mktemp -t lighter-m8-late)"
python3 - "$LATE_FILE" <<'PY' &
import socket, sys, time
s = socket.socket()
s.bind(("0.0.0.0", 0))
s.listen(1)
open(sys.argv[1], "w").write(str(s.getsockname()[1]))
c = s.accept()[0]
while c.recv(4096):
    pass
time.sleep(75)
c.sendall(b"late")
c.close()
PY
LATE_SERVER=$!
for _ in $(seq 1 50); do [ -s "$LATE_FILE" ] && break; sleep 0.1; done
LATE_PORT="$(cat "$LATE_FILE")"
docker pull -q python:3.12-slim >/dev/null
docker run --rm python:3.12-slim python3 -c "
import socket
c = socket.create_connection(('host.docker.internal', $LATE_PORT))
c.sendall(b'request')
c.shutdown(socket.SHUT_WR)
c.settimeout(150)
print(c.recv(16).decode())" > "$LATE_FILE" 2>&1 &
LATE_CLIENT=$!
before="$(proxy_fds)"
# All at once: each `nc -w 1` waits out its second on a server that never
# closes, and two hundred in turn would take long enough for the proxy to
# let the first go before the count. Its exit status says it gave up, which
# is not what is measured.
docker run --rm alpine:3.21 sh -c "for i in \$(seq 1 200); do printf x | nc -w 1 host.docker.internal $NC_PORT >/dev/null 2>&1 & done; wait; true"
held=$(( $(proxy_fds) - before ))
started="$(date +%s)"
left="$held"
while [ "$left" -gt 8 ] && [ $(( $(date +%s) - started )) -lt 180 ]; do
	sleep 5
	left=$(( $(proxy_fds) - before ))
done
if [ "$held" -lt 400 ]; then
	fail "the proxy held only $held descriptors for 200 half-closed streams; the check did not exercise them"
elif [ "$left" -le 8 ]; then
	pass "200 streams the container closed, to a server that never does, let go within $(( $(date +%s) - started ))s ($held descriptors back)"
else
	fail "the proxy still holds $left of $held descriptors after 180s"
fi
kill "$NEVER_CLOSES" 2>/dev/null
NEVER_CLOSES=""
wait "$LATE_CLIENT" 2>/dev/null || true
if [ "$(cat "$LATE_FILE")" = late ]; then
	pass "a container that only shut down its writes still got a reply 75s later"
else
	fail "a half-closed stream lost its late reply: $(tail -1 "$LATE_FILE")"
fi
kill "$LATE_SERVER" 2>/dev/null || true
rm -f "$LATE_FILE"

echo
echo "==> What a closed lid does"
# Exactly what a suspend does to a guest with no real-time clock.
skewed=$(( $(date +%s) - 3600 ))
docker run --rm --privileged alpine:3.21 date -u -s "@$skewed" >/dev/null 2>&1 || true
guest_hour() { docker run --rm alpine:3.21 date -u +%s 2>/dev/null; }
before="$(guest_hour)"
drift=$(( $(date -u +%s) - ${before:-0} ))
if [ "$drift" -gt 1800 ]; then
	pass "the guest clock is now ${drift}s behind, as a sleep would leave it"
else
	fail "could not skew the guest clock (drift ${drift}s)"
fi

# The same path the wake notification runs.
if "$LIGHTER" resync >/dev/null 2>&1; then
	after="$(guest_hour)"
	drift=$(( $(date -u +%s) - ${after:-0} ))
	drift=${drift#-}
	if [ "$drift" -le 5 ]; then
		pass "waking put the clock right (within ${drift}s)"
	else
		fail "the clock is still ${drift}s out after resync"
	fi
else
	fail "lighter resync failed"
fi

echo
echo "==> Everything still works afterwards"
if docker compose -f "$COMPOSE" exec -T postgres pg_isready -U postgres </dev/null >/dev/null 2>&1; then
	pass "postgres survived it"
else
	fail "postgres did not survive it"
fi
check_port "mailpit" "http://127.0.0.1:18025/"
# TLS is what a wrong clock breaks first, and it breaks by blaming the
# certificate. This is the check that a wrong clock would fail.
if docker run --rm alpine:3.21 sh -c 'apk add --no-cache -q ca-certificates >/dev/null 2>&1; wget -q -O /dev/null https://example.com/' >/dev/null 2>&1; then
	pass "a container completed a TLS handshake"
else
	fail "TLS from a container failed, which is what a wrong clock looks like"
fi

echo
echo "==> Stopping"
if "$LIGHTER" stop >/dev/null 2>&1 && ! "$LIGHTER" status >/dev/null 2>&1; then
	pass "lighter stop left nothing running"
else
	fail "lighter stop did not stop it"
fi

echo
echo "==> Starting without a local Docker CLI"
if PATH=/usr/bin:/bin:/usr/sbin:/sbin "$LIGHTER" start >"$LIGHTER_HOME/no-docker.log" 2>&1 \
	&& [ "$(curl -fsS --max-time 10 --unix-socket "$LIGHTER_HOME/docker.sock" http://localhost/_ping)" = OK ]; then
	pass "the machine starts and serves its API without docker on PATH"
else
	fail "start requires a local Docker CLI"
	tail -10 "$LIGHTER_HOME/no-docker.log" | sed 's/^/    /'
fi
"$LIGHTER" stop >/dev/null 2>&1 || fail "the machine without a local CLI did not stop"

echo
if [ "$FAILED" -eq 0 ]; then
	printf '\033[32mmilestone 8 gate passed\033[0m — a day of work, and a night of sleep.\n'
	exit 0
fi
printf '\033[31mmilestone 8 gate failed\033[0m\n'
exit 1
