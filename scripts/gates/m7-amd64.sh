#!/usr/bin/env bash
# Milestone 7 gate: x86-64 containers run — under Rosetta when the Mac has it.
#
# An amd64 image runs, reports the right architecture, and executes a real
# program rather than just `uname`. On a Mac with Rosetta installed the guest
# has to have registered Rosetta, the kernel has to have found the per-thread
# memory-ordering switch Rosetta asks for, and an x86-64 multi-threaded program
# has to see x86's ordering — see `docs/x86-64.md`. On a Mac without it there
# is no emulator: the guest registers a handler that names the fix, and an
# amd64 container has to fail with that message rather than "exec format
# error".
set -euo pipefail

if ! command -v cargo >/dev/null 2>&1; then
	# shellcheck disable=SC1091
	. "$HOME/.cargo/env"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

KERNEL="${LIGHTER_GATE_KERNEL:-guest/out/Image}"
# A private clone, not the master: the master is an artifact, and any second
# machine mounting it read-write beside the first corrupts both.
ROOTFS_MASTER="guest/out/rootfs.ext4"
ROOTFS="$(mktemp -t lighter-rootfs).ext4"
cp -c "$ROOTFS_MASTER" "$ROOTFS" 2>/dev/null || cp "$ROOTFS_MASTER" "$ROOTFS"
PROFILE="${PROFILE:-release}"
BIN="target/$PROFILE/examples/lighter-bench"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-180}"

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
note() { printf '  \033[33m··\033[0m   %s\n' "$*"; }
FAILED=0

command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }

echo "==> Building guest artifacts if missing"
[ -f "$KERNEL" ] || ./guest/kernel/build.sh
[ -f "$ROOTFS" ] || ./guest/rootfs/build.sh

echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

RUN_DIR="$(mktemp -d -t lighter-m7)"
SOCKET="$RUN_DIR/docker.sock"
LOG="$RUN_DIR/boot.log"
VMM_PID=""

cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	rm -rf "$RUN_DIR"
	rm -f "${ROOTFS:-}"
}
trap cleanup EXIT
trap 'exit 143' INT TERM

ROSETTA_DIR=/Library/Apple/usr/libexec/oah/RosettaLinux
if [ -x "$ROSETTA_DIR/rosetta" ]; then
	VIA=rosetta
	ROSETTA_ARGS=(--share "rosetta:$ROSETTA_DIR")
	ROSETTA_CMDLINE=" lighter.rosetta"
else
	VIA=hint
	ROSETTA_ARGS=()
	ROSETTA_CMDLINE=""
	note "Rosetta is not installed on this Mac; checking that amd64 says so"
fi

echo
echo "==> Booting"
: > "$LOG"
"$BIN" \
	--kernel "$KERNEL" \
	--disk "$ROOTFS" \
	--disk "$RUN_DIR/data.img" --disk-size-gib 32 \
	--net --run-dir "$RUN_DIR" \
	--vsock "$SOCKET:2375" \
	"${ROSETTA_ARGS[@]}" \
	--no-tty --cpus 4 --memory-mib 4096 \
	--cmdline "console=ttyAMA0 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s)$ROSETTA_CMDLINE" \
	>"$LOG" 2>&1 &
VMM_PID=$!
disown "$VMM_PID" 2>/dev/null || true

waited=0
while ! grep -q "AGENT listening" "$LOG" 2>/dev/null; do
	kill -0 "$VMM_PID" 2>/dev/null || { fail "the VMM exited during boot"; tail -20 "$LOG" | sed 's/^/    /'; exit 1; }
	[ "$waited" -lt "$BOOT_TIMEOUT" ] || { fail "the guest did not come up"; exit 1; }
	sleep 1
	waited=$((waited + 1))
done
export DOCKER_HOST="unix://$SOCKET"
pass "guest booted in ${waited}s"

if grep -q "INIT binfmt=x86_64 via=$VIA" "$LOG"; then
	pass "the guest registered $VIA as the x86-64 handler"
else
	fail "the guest did not register $VIA"
	grep -a "INIT binfmt\|INIT rosetta" "$LOG" | sed 's/^/    /'
fi
if [ "$VIA" = rosetta ]; then
	if grep -q "Apple TSO: available per thread" "$LOG"; then
		pass "the kernel found the per-thread memory-ordering switch"
	else
		fail "the kernel did not find the per-thread memory-ordering switch"
		grep -a "Apple TSO" "$LOG" | sed 's/^/    /'
	fi
fi

echo
echo "==> Running amd64 containers"
# Pulled by digest-free tag but with an explicit platform, and *pinned to
# different local tags*. A plain `docker pull --platform linux/amd64
# alpine:3.21` replaces whatever `alpine:3.21` meant locally, so a later
# "native" check silently runs the amd64 image and reports x86_64 — which reads
# as a broken binfmt registration rather than a broken benchmark.
docker pull --quiet --platform linux/amd64 alpine:3.21 >/dev/null 2>&1 || true
docker tag alpine:3.21 lighter-test:amd64 >/dev/null 2>&1 || true
docker pull --quiet --platform linux/arm64 alpine:3.21 >/dev/null 2>&1 || true
docker tag alpine:3.21 lighter-test:arm64 >/dev/null 2>&1 || true

if [ "$VIA" = hint ]; then
	# No Rosetta: the run has to fail, and fail saying what to install.
	said="$(docker run --rm lighter-test:amd64 uname -m 2>&1 || true)"
	case "$said" in
	*"lighter rosetta --install"*)
		pass "an amd64 container fails naming the fix: ${said%%. *}"
		;;
	*)
		fail "an amd64 container without Rosetta said '${said:-nothing}'"
		;;
	esac
else
	arch="$(docker run --rm lighter-test:amd64 uname -m 2>/dev/null || true)"
	if [ "$arch" = "x86_64" ]; then
		pass "an amd64 container reports $arch"
	else
		fail "an amd64 container reported '${arch:-nothing}'"
	fi

	# `uname` is a syscall and would work under a handler that did nothing
	# useful. A real interpreter has to translate an actual program.
	docker pull --quiet --platform linux/amd64 node:24-alpine >/dev/null 2>&1 || true
	answer="$(docker run --rm --platform linux/amd64 node:24-alpine \
		node -e 'const os=require("os");
		         let n=0; for (let i=0;i<2e6;i++) n=(n+i*7)%1000003;
		         console.log(os.arch()+" "+n)' 2>/dev/null || true)"
	case "$answer" in
	"x64 "*)
		pass "an amd64 Node image ran a real program: $answer"
		;;
	*)
		fail "the amd64 Node image produced '${answer:-nothing}'"
		;;
	esac
fi

# Making sure the native path did not regress while adding the emulated one.
native="$(docker run --rm lighter-test:arm64 uname -m 2>/dev/null || true)"
if [ "$native" = "aarch64" ]; then
	pass "native containers still report $native"
else
	fail "a native container reported '${native:-nothing}'"
fi

if [ "$VIA" = rosetta ]; then
	# The ordering Rosetta's threads run with, seen from the x86 side: a
	# writer stores x then y, a reader loads y then x, and x86 forbids
	# seeing the second store without the first. Compiled by the amd64
	# image's own gcc, under Rosetta, and run for two million rounds; ARM's
	# ordering shows a handful of reorderings in that many, x86's none.
	litmus="$(docker run --rm --platform linux/amd64 lighter-test:amd64 sh -c '
		apk add -q gcc musl-dev >/dev/null 2>&1 || exit 9
		cat > /mp.c <<"EOF"
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <sched.h>
#include <stdatomic.h>
static volatile int x, y;
static atomic_long bar1, bar2;
static long iters = 2000000, hits;
static void pin(int cpu){cpu_set_t s;CPU_ZERO(&s);CPU_SET(cpu,&s);sched_setaffinity(0,sizeof s,&s);}
static void spin(unsigned n){for(volatile unsigned k=0;k<n;k++);}
static void* writer(void*a){pin(1);unsigned r=1;for(long i=0;i<iters;i++){
  atomic_fetch_add(&bar1,1);while(atomic_load(&bar1)<2*(i+1));
  r=r*1103515245u+12345u;spin((r>>16)&63);x=1;y=1;
  atomic_fetch_add(&bar2,1);while(atomic_load(&bar2)<2*(i+1));}return 0;}
int main(void){pin(2);pthread_t t;pthread_create(&t,0,writer,0);unsigned r=7;
for(long i=0;i<iters;i++){x=0;y=0;
  atomic_fetch_add(&bar1,1);while(atomic_load(&bar1)<2*(i+1));
  r=r*1103515245u+12345u;spin((r>>16)&63);
  int ry=y;int rx=x;if(ry==1&&rx==0)hits++;
  atomic_fetch_add(&bar2,1);while(atomic_load(&bar2)<2*(i+1));}
pthread_join(t,0);printf("%ld\n",hits);return 0;}
EOF
		gcc -O2 -pthread -o /mp /mp.c && timeout 120 /mp' 2>/dev/null || echo error)"
	case "$litmus" in
	0) pass "x86-64 threads under Rosetta see x86 memory ordering (0 reorderings in 2M rounds)" ;;
	error | "") fail "the x86-64 litmus test did not run" ;;
	*) fail "x86-64 threads under Rosetta saw $litmus reorderings in 2M rounds" ;;
	esac
fi

echo
if [ "$FAILED" -eq 0 ]; then
	printf '\033[32mmilestone 7 gate passed\033[0m — x86-64 containers run under '"$VIA"'.\n'
	exit 0
fi
printf '\033[31mmilestone 7 gate failed\033[0m — log at %s\n' "$LOG"
cp "$LOG" /tmp/lighter-m7.log 2>/dev/null || true
exit 1
