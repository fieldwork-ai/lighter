#!/usr/bin/env bash
# Milestone 3 gate: Docker, end to end, driven by the real macOS client.
#
# The claim being tested is the product's whole reason to exist — that `docker`
# on a Mac talks to a daemon in a VM we wrote, and that a developer stack comes
# up and is reachable. Nothing here is a stand-in: it is the Docker CLI, Docker
# Compose, real images from Docker Hub, and curl from macOS.
#
#   docker version    the daemon answers over vsock
#   hello-world       registry pull, TLS against a clock we set, container run,
#                     and the attached output stream
#   compose up        three services, health checks green, a named volume
#   ports             published ports reachable from macOS, opened by nothing
#                     more than starting the container
#   persistence       a named volume survives a container restart
#   withdrawal        stopping a container closes its door again
set -euo pipefail

# cargo lives in ~/.cargo/bin, which a non-login shell does not have on PATH —
# a launchd job, a CI step, an editor terminal. Without this the gate fails at
# "cargo: command not found" and looks like a broken toolchain.
if ! command -v cargo >/dev/null 2>&1; then
	# shellcheck disable=SC1091
	. "$HOME/.cargo/env"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

KERNEL="guest/out/Image"
# A private clone, not the master: the master is an artifact, and any second
# machine mounting it read-write beside the first corrupts both.
ROOTFS_MASTER="guest/out/rootfs.ext4"
ROOTFS="$(mktemp -t lighter-rootfs).ext4"
cp -c "$ROOTFS_MASTER" "$ROOTFS" 2>/dev/null || cp "$ROOTFS_MASTER" "$ROOTFS"
COMPOSE="scripts/gates/fixtures/compose.yml"
PROFILE="${PROFILE:-debug}"
BIN="target/$PROFILE/examples/lighter-bench"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-120}"

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
FAILED=0

for tool in docker curl; do
	command -v "$tool" >/dev/null 2>&1 || { echo "$tool is required" >&2; exit 1; }
done

echo "==> Building guest artifacts if missing"
[ -f "$KERNEL" ] || ./guest/kernel/build.sh
[ -f "$ROOTFS" ] || ./guest/rootfs/build.sh

echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

RUN_DIR="$(mktemp -d -t lighter-m3d)"
SOCKET="$RUN_DIR/docker.sock"
DATA="$RUN_DIR/data.img"
LOG="$(mktemp -t lighter-m3d-log)"
VMM_PID=""
export DOCKER_HOST="unix://$SOCKET"

cleanup() {
	docker compose -f "$COMPOSE" down -v --timeout 5 >/dev/null 2>&1 || true
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	rm -rf "$RUN_DIR"
}
trap cleanup EXIT

echo
echo "==> Booting the Docker guest"
# The host's clock is passed in because the machine has no RTC, and a guest at
# the epoch fails every registry TLS handshake on a certificate "not yet valid".
"$BIN" \
	--kernel "$KERNEL" \
	--disk "$ROOTFS" \
	--disk "$DATA" --disk-size-gib 16 \
	--net --run-dir "$RUN_DIR" \
	--vsock "$SOCKET:2375" \
	--docker-ports "$SOCKET" \
	--no-tty --cpus 4 --memory-mib 4096 \
	--cmdline "console=ttyAMA0 earlycon=pl011,0xc000000 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s)" \
	>"$LOG" 2>&1 &
VMM_PID=$!

waited=0
while ! grep -q "AGENT listening" "$LOG" 2>/dev/null; do
	if ! kill -0 "$VMM_PID" 2>/dev/null; then
		fail "the VMM exited during boot"
		tail -20 "$LOG" | sed 's/^/    /'
		exit 1
	fi
	if [ "$waited" -ge "$BOOT_TIMEOUT" ]; then
		fail "the guest agent did not come up within ${BOOT_TIMEOUT}s"
		tail -20 "$LOG" | sed 's/^/    /'
		exit 1
	fi
	sleep 1
	waited=$((waited + 1))
done
pass "guest booted and the docker socket is served (${waited}s)"

# dockerd starting is not the same as dockerd surviving: it creates its socket
# early and can die later, which reads as a bare connection refused.
grep -q "INIT dockerd=ready" "$LOG" && pass "dockerd came up" || fail "dockerd did not start"

if server="$(docker version --format '{{.Server.Version}} on {{.Server.Os}}/{{.Server.Arch}}' 2>&1)"; then
	pass "docker client reached the daemon: $server"
else
	fail "docker version failed: $server"
fi

# The output has to come back, not just the exit code: `docker run` streams it
# over a hijacked connection that a half-close bug silently kills.
if out="$(docker run --rm hello-world 2>&1)"; then
	if grep -q "installation appears to be working correctly" <<<"$out"; then
		pass "docker run hello-world produced its output"
	else
		fail "hello-world ran but produced no output (attach stream lost?)"
	fi
else
	fail "docker run hello-world failed"
	tail -5 <<<"$out" | sed 's/^/    /'
fi

echo
echo "==> Bringing up a Fieldwork-shaped stack"
if docker compose -f "$COMPOSE" up -d --wait --wait-timeout 300 >/dev/null 2>&1; then
	pass "compose up: all services reported healthy"
else
	fail "compose up did not reach a healthy state"
	docker compose -f "$COMPOSE" ps 2>&1 | sed 's/^/    /'
fi

# Published ports, reached from macOS. Nothing asked for these forwards: they
# exist because a container started, which is the behaviour being tested.
# curl already writes 000 to %{http_code} when it cannot connect, so its exit
# status is deliberately swallowed rather than turned into a second 000 — an
# `|| echo 000` here yields "000000", and a correctly closed port then reads as
# one that is still answering.
code() { curl -s --max-time 10 -o /dev/null -w '%{http_code}' "$1" 2>/dev/null || true; }

[ "$(code http://127.0.0.1:18025/readyz)" = 200 ] \
	&& pass "mailpit reachable on 127.0.0.1:18025" \
	|| fail "mailpit not reachable on 127.0.0.1:18025"

[ "$(code http://127.0.0.1:19000/minio/health/live)" = 200 ] \
	&& pass "minio reachable on 127.0.0.1:19000" \
	|| fail "minio not reachable on 127.0.0.1:19000"

# Postgres over its own wire protocol rather than a TCP knock, because a port
# that accepts and then does nothing useful is the failure worth catching.
pg() { PGPASSWORD=lighter psql -h 127.0.0.1 -p 15434 -U postgres -d lighter -tAc "$1" 2>&1; }
if command -v psql >/dev/null 2>&1; then
	if version="$(pg 'select version()')" && grep -q PostgreSQL <<<"$version"; then
		pass "postgres speaks its protocol on 127.0.0.1:15434"
	else
		fail "postgres did not answer: $version"
	fi

	# A named volume on the data disk has to outlive the container using it.
	pg 'create table if not exists gate(x int); insert into gate values (42);' >/dev/null
	docker compose -f "$COMPOSE" restart postgres >/dev/null 2>&1
	sleep 10
	if [ "$(pg 'select count(*) from gate')" = "1" ]; then
		pass "named volume survived a container restart"
	else
		fail "data did not survive a restart of the container"
	fi
else
	printf '  \033[33mskip\033[0m psql not installed; postgres checked by port only\n'
	nc -z -G 5 127.0.0.1 15434 && pass "postgres port open" || fail "postgres port closed"
fi

# Stopping a container must close its door again, or a stale forward outlives
# what it pointed at.
docker compose -f "$COMPOSE" stop mailpit >/dev/null 2>&1
sleep 5
[ "$(code http://127.0.0.1:18025/readyz)" = 000 ] \
	&& pass "forward withdrawn when the container stopped" \
	|| fail "18025 still answers after mailpit stopped"

docker compose -f "$COMPOSE" down -v --timeout 10 >/dev/null 2>&1 && pass "compose down cleaned up" || fail "compose down failed"

for signature in "Kernel panic" "Internal error: Oops" "INIT dockerd=exited"; do
	if grep -qF "$signature" "$LOG"; then
		fail "guest reported: $signature"
		grep -F -m1 -A5 "$signature" "$LOG" | sed 's/^/    /'
	fi
done

echo
if [ "$FAILED" -eq 0 ]; then
	printf '\033[32mmilestone 3 docker gate passed\033[0m — docker on macOS, on a VMM we wrote.\n'
	rm -f "$LOG"
	exit 0
fi
printf '\033[31mmilestone 3 docker gate failed\033[0m — log at %s\n' "$LOG"
exit 1
