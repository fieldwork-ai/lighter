#!/usr/bin/env bash
# Milestone 3 gate, part one: the guest is on the network, in both directions.
#
# Docker is useless without this and hard to debug through, so networking is
# proven on its own first. Each check pins a different half of the path:
#
#   link      — the device tree slot became an interface with our MAC
#   dhcp      — a broadcast went out the transmit queue and a reply came back
#               in through the receive queue, which is the whole virtio-net
#               round trip with nothing else involved
#   gateway   — ICMP to gvproxy itself, so a failure here is ours and not the
#               internet's
#   dns       — resolution through gvproxy, which follows the Mac's own resolver
#   egress    — a real TCP connection terminated on a host socket
#   forward   — the other direction: macOS connects to a guest listener through
#               a forward added at runtime over gvproxy's control socket
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
GVPROXY="${GVPROXY:-vendor/gvproxy}"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-90}"
PROFILE="${PROFILE:-debug}"
BIN="target/$PROFILE/examples/lighter-bench"

# High and unprivileged, and not a port anything else on a developer's Mac is
# likely to be sitting on.
HOST_PORT="${HOST_PORT:-18080}"
GUEST_PORT=8000

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
FAILED=0

if [ ! -x "$GVPROXY" ]; then
	echo "gvproxy not found at $GVPROXY; run scripts/fetch-gvproxy.sh" >&2
	exit 1
fi

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
	--net "$GVPROXY" \
	--run-dir "$RUN_DIR" \
	--forward "$HOST_PORT:$GUEST_PORT" \
	--cmdline "console=ttyAMA0 earlycon=pl011,0xc000000 panic=-1 lighter.nettest=listen" \
	>"$LOG" 2>&1 &
VMM_PID=$!

# The guest gets to the listener only after DHCP, ping, DNS and an HTTP fetch,
# so waiting for the marker rather than a fixed sleep is both faster and immune
# to a slow network making the gate flaky.
waited=0
while ! grep -q "NETTEST listening=$GUEST_PORT" "$LOG" 2>/dev/null; do
	if ! kill -0 "$VMM_PID" 2>/dev/null; then
		break
	fi
	if [ "$waited" -ge "$BOOT_TIMEOUT" ]; then
		fail "guest did not reach its listener within ${BOOT_TIMEOUT}s"
		break
	fi
	sleep 1
	waited=$((waited + 1))
done

# Inbound, from macOS, while the guest is still up.
if grep -q "NETTEST listening=$GUEST_PORT" "$LOG" 2>/dev/null; then
	reply="$(nc -w 5 127.0.0.1 "$HOST_PORT" 2>/dev/null || true)"
	if [ "$reply" = "hello from lighter" ]; then
		pass "host reached the guest through 127.0.0.1:$HOST_PORT"
	else
		fail "port forward returned ${reply:-nothing}"
	fi
fi

# The guest powers itself off once served; give it a moment before judging.
waited=0
while kill -0 "$VMM_PID" 2>/dev/null && [ "$waited" -lt 30 ]; do
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

grep -q "NETTEST gateway_ping=ok" "$LOG" && pass "gateway reachable"  || fail "cannot ping 192.168.127.1"
grep -q "NETTEST dns=ok"          "$LOG" && pass "DNS resolves"       || fail "DNS did not resolve"

if grep -q "NETTEST egress=ok" "$LOG"; then
	pass "TCP egress works ($(grep -o 'NETTEST egress=ok bytes=[0-9]*' "$LOG" | cut -d= -f3) bytes fetched)"
else
	fail "no TCP egress"
fi

grep -q "NETTEST served=ok" "$LOG" && pass "guest served the forwarded connection" || fail "guest listener never completed"
grep -q "NETTEST done"      "$LOG" && pass "nettest completed"                     || fail "nettest did not finish"

# A frame-length mismatch or a wrong header size shows up here long before it
# shows up as a failed check, because it takes the stream down permanently.
if grep -q "network stream framing lost" "$LOG"; then
	fail "the gvproxy stream desynchronized"
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
