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
# here: the clock is skewed by an hour through the same control channel the
# host uses, and then the same recovery path that `IOKit`'s wake notification
# triggers is run. Everything is exercised except IOKit's delivery of the
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

cleanup() {
	docker compose -f "$COMPOSE" down -v --timeout 10 >/dev/null 2>&1 || true
	rm -rf "$SHARE"
}
trap cleanup EXIT

echo "==> Building and signing the CLI"
cargo build $([ "$PROFILE" = release ] && echo --release) -p lighter-cli
./scripts/sign.sh "$LIGHTER" >/dev/null

# The stack bind-mounts this, and nginx has to find something to serve.
echo "<h1>lighter</h1>" > "$SHARE/index.html"

echo
echo "==> Starting the way a person does"
"$LIGHTER" stop >/dev/null 2>&1 || true
if "$LIGHTER" start >/dev/null 2>&1; then
	pass "lighter start brought up a machine and pointed docker at it"
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
if [ "$(docker context show 2>/dev/null)" = lighter ]; then
	pass "the docker CLI points at lighter by default"
else
	fail "the docker context was not selected"
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

# stdin from /dev/null, not inherited: `compose exec` attaches it even with
# -T, so a gate run from a pipe that never closes waits forever on input
# nobody is going to type. It hung here for an hour before that was obvious.
if docker compose -f "$COMPOSE" exec -T postgres pg_isready -U postgres </dev/null >/dev/null 2>&1; then
	pass "postgres is serving on its named volume"
else
	fail "postgres is not answering"
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
printf 'time %s\n' "$skewed" | nc -U "$HOME/.lighter/control.sock" -w 2 >/dev/null 2>&1 || true
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
