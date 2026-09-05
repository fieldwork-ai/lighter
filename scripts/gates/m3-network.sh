#!/usr/bin/env bash
# Milestone 3 gate, part one: the network card, and what the VMM answers on it.
#
# Everything a container uses a network for — TCP, UDP, DNS, published ports —
# is a stream over vsock and never touches the card; the streams gate proves
# those. What the card still carries is what the guest needs to believe it is
# on a network, and each check pins one of them:
#
#   link      — the device tree slot became an interface with our MAC
#   dhcp      — a broadcast went out the transmit queue and the responder's
#               lease came back in through the receive queue, which is the
#               whole virtio-net round trip with nothing else involved
#   gateway   — ICMP to the responder itself, so a failure here is ours
#   quiet     — nothing else reached the card: a TCP or UDP frame there is a
#               flow that escaped the redirects
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

KERNEL="${LIGHTER_GATE_KERNEL:-guest/out/Image}"
INITRAMFS="guest/out/initramfs.cpio.gz"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-90}"
PROFILE="${PROFILE:-debug}"
BIN="target/$PROFILE/examples/lighter-bench"

# High and unprivileged, and not a port anything else on a developer's Mac is
# likely to be sitting on.

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
FAILED=0


echo "==> Building guest artifacts if missing"
[ -f "$KERNEL" ]    || ./guest/kernel/build.sh
[ -f "$INITRAMFS" ] || ./guest/initramfs/build.sh

echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

RUN_DIR="$(mktemp -d -t lighter-m3)"
LOG="$(mktemp -t lighter-m3-log)"
VMM_PID=""

cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	rm -rf "$RUN_DIR"
}
trap cleanup EXIT

echo
echo "==> Booting with networking"
"$BIN" \
	--kernel "$KERNEL" \
	--initramfs "$INITRAMFS" \
	--no-tty \
	--cpus 2 \
	--net \
	--run-dir "$RUN_DIR" \
	--cmdline "console=ttyAMA0 earlycon=pl011,0xc000000 panic=-1 lighter.nettest" \
	>"$LOG" 2>&1 &
VMM_PID=$!

# The guest gets to the listener only after DHCP, ping, DNS and an HTTP fetch,
# so waiting for the marker rather than a fixed sleep is both faster and immune
# to a slow network making the gate flaky.
# The guest powers itself off once done; give it that long before judging.
waited=0
while kill -0 "$VMM_PID" 2>/dev/null && [ "$waited" -lt "$BOOT_TIMEOUT" ]; do
	sleep 1
	waited=$((waited + 1))
done
kill -9 "$VMM_PID" 2>/dev/null || true
VMM_PID=""

grep -q "NETTEST link=eth0 mac=5a:94:ef:e4:0c:ee" "$LOG" \
	&& pass "virtio-net bound with the expected MAC" \
	|| fail "no eth0: $(grep -o 'NETTEST link=.*' "$LOG" || echo 'nothing reported')"

grep -q "NETTEST dhcp=ok addr=192.168.127.2/24" "$LOG" \
	&& pass "DHCP leased 192.168.127.2" \
	|| fail "DHCP: $(grep -o 'NETTEST dhcp=.*' "$LOG" || echo 'nothing reported')"

grep -q "NETTEST gateway_ping=ok" "$LOG" && pass "gateway answers ICMP" || fail "cannot ping 192.168.127.1"
grep -q "NETTEST done"            "$LOG" && pass "nettest completed"    || fail "nettest did not finish"

# A frame the responder had no answer for is logged at debug; TCP or UDP
# there would be a flow escaping the redirects.
if grep -q "frame with no answer dropped" "$LOG"; then
	fail "the card saw a frame it should not have: $(grep -m1 -o 'frame with no answer dropped.*' "$LOG")"
fi

for signature in "Kernel panic" "Internal error: Oops" "Unable to handle kernel"; do
	if grep -qF "$signature" "$LOG"; then
		fail "guest reported: $signature"
		grep -F -m1 -A3 "$signature" "$LOG" | sed 's/^/    /'
	fi
done

echo
if [ "$FAILED" -eq 0 ]; then
	printf '\033[32mmilestone 3 network gate passed\033[0m\n'
	rm -f "$LOG"
	exit 0
fi
printf '\033[31mmilestone 3 network gate failed\033[0m — log at %s\n' "$LOG"
exit 1
