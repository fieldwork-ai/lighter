#!/usr/bin/env bash
# Milestone 3 gate, part two: a host socket reaches a guest process over the link.
#
# This is the path the Docker socket takes, proven with an echo server instead
# of dockerd so that a failure here is unambiguously the transport:
#
#   docker CLI ──unix──▶ lighter ──link──▶ agent ──unix──▶ dockerd
#                        └───────── this gate ────────┘
#
# The round trip exercises both directions and the credit accounting: the host's
# bytes reach the guest through the RX queue and come back through TX.
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
INITRAMFS="guest/out/initramfs.cpio.gz"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-90}"
PROFILE="${PROFILE:-debug}"
BIN="target/$PROFILE/examples/lighter-bench"
GUEST_PORT=2375

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
FAILED=0

echo "==> Building guest artifacts if missing"
[ -f "$KERNEL" ]    || ./guest/kernel/build.sh
[ -f "$INITRAMFS" ] || ./guest/initramfs/build.sh

echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

SOCKET="$(mktemp -u -t lighter-vsock).sock"
LOG="$(mktemp -t lighter-m3v)"
VMM_PID=""

cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	rm -f "$SOCKET"
}
trap cleanup EXIT

echo
echo "==> Booting with a socket proxied to guest port $GUEST_PORT"
"$BIN" \
	--kernel "$KERNEL" \
	--initramfs "$INITRAMFS" \
	--no-tty \
	--cpus 2 \
	--net --run-dir "$(mktemp -d -t lighter-m3v)" \
	--proxy "$SOCKET:$GUEST_PORT" \
	--cmdline "console=hvc0 panic=-1 lighter.vsocktest" \
	>"$LOG" 2>&1 &
VMM_PID=$!

# The agent prints its listening line only after bind(2) returns, so this is the
# point at which a connection would be accepted rather than refused.
waited=0
while ! grep -q "AGENT listening port=$GUEST_PORT" "$LOG" 2>/dev/null; do
	if ! kill -0 "$VMM_PID" 2>/dev/null; then
		fail "the VMM exited before the agent was listening"
		break
	fi
	if [ "$waited" -ge "$BOOT_TIMEOUT" ]; then
		fail "the guest agent did not bind within ${BOOT_TIMEOUT}s"
		break
	fi
	sleep 1
	waited=$((waited + 1))
done

if grep -q "AGENT listening port=$GUEST_PORT" "$LOG" 2>/dev/null; then
	pass "guest agent bound port $GUEST_PORT"

	[ -S "$SOCKET" ] && pass "host socket exists" || fail "no socket at $SOCKET"

	# stdin is held open deliberately. `echo x | nc -U` closes as soon as the
	# pipe drains, which tears the connection down before the reply arrives and
	# looks exactly like a broken transport.
	MESSAGE="the quick brown fox jumps over the lazy dog"
	reply="$( (echo "$MESSAGE"; sleep 3) | nc -U -w 6 "$SOCKET" 2>/dev/null | head -1 || true)"
	if [ "$reply" = "$MESSAGE" ]; then
		pass "round trip through the link returned the message intact"
	else
		fail "expected the message back, got: ${reply:-nothing}"
	fi

	# Larger than one 64 KiB packet and larger than the 256 KiB credit window,
	# so the transfer cannot complete without the peer's credit updates being
	# read and acted on. A broken window stalls here rather than corrupting.
	big="$(mktemp -t lighter-big)"
	# Generated without a pipeline on purpose: `... | head -c N` gives the
	# upstream process SIGPIPE, and under `set -o pipefail` that aborts the
	# gate with no message. Deterministic content also makes a corrupt result
	# reproducible rather than a one-off.
	awk 'BEGIN { line = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
	             for (i = 0; i < 12500; i++) printf "%s", line }' > "$big"
	got="$( (cat "$big"; sleep 5) | nc -U -w 20 "$SOCKET" 2>/dev/null | wc -c | tr -d ' ' || true)"
	if [ "${got:-0}" -ge 999000 ]; then
		pass "1 MB round trip returned $got bytes (credit window respected)"
	else
		fail "1 MB round trip returned ${got:-0} bytes; expected ~1000000"
	fi
	rm -f "$big"
fi

# The guest powers off on its own; give it a moment before judging the log.
waited=0
while kill -0 "$VMM_PID" 2>/dev/null && [ "$waited" -lt 30 ]; do
	sleep 1
	waited=$((waited + 1))
done
kill -9 "$VMM_PID" 2>/dev/null || true
VMM_PID=""

grep -q "VSOCKTEST device=ok" "$LOG" && pass "virtio-vsock probed in the guest" || fail "no /dev/vsock"

# A packet the guest could not fit is a stall, and it is silent from outside.
if grep -q "receive buffer too small" "$LOG"; then
	fail "a vsock packet did not fit the guest's receive buffer"
fi

for signature in "Kernel panic" "Internal error: Oops" "Unable to handle kernel"; do
	if grep -qF "$signature" "$LOG"; then
		fail "guest reported: $signature"
		grep -F -m1 -A3 "$signature" "$LOG" | sed 's/^/    /'
	fi
done

echo
if [ "$FAILED" -eq 0 ]; then
	printf '\033[32mmilestone 3 vsock gate passed\033[0m\n'
	rm -f "$LOG"
	exit 0
fi
printf '\033[31mmilestone 3 vsock gate failed\033[0m — log at %s\n' "$LOG"
exit 1
