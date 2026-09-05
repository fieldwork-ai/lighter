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

cleanup() {
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
if [ "$FAILED" -eq 0 ]; then
	printf '\033[32mmilestone 8 gate passed\033[0m — a day of work, and a night of sleep.\n'
	exit 0
fi
printf '\033[31mmilestone 8 gate failed\033[0m\n'
exit 1
