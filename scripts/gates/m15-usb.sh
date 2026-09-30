#!/usr/bin/env bash
# Milestone 15 gate, part one: USB devices on the Mac, in the guest.
#
# Hardware: a Home Assistant Connect ZBT-2 (303a:831a, CDC-ACM, EmberZNet
# firmware) and a ThirdReality Zigbee dongle (1a86:7523, a CH340 bridge to a
# BL706 speaking BLZ at 2 Mbaud), plugged into this Mac. Between them they
# cover both of Apple's driver models (a kernel driver and a DriverKit one)
# and both of Linux's (cdc_acm and a usb-serial bridge). Nothing is flashed:
# each radio is asked its firmware version, which reads and changes nothing.
#
#   attach     — both attach, from the configuration, at start
#   names      — /dev/serial/by-id as udev names them
#   macos      — macOS's own ports are gone while the guest has the devices
#   radios     — each radio answers through a container, at native speed
#   detach     — macOS has the device back, and the guest has it no longer
#   replug     — a device unplugged on the guest's side comes back on its own
#   agent      — the agent restarted under an attached device keeps its names
#   in use     — a port a Mac program holds is refused, naming the program
#   restart    — a machine started again attaches its devices again
#   crash      — a machine killed holding them gives them back to macOS
#
# Pairing a device and unplugging a stick by hand are part two, with a
# person at the Mac (docs/release-notes-0.11.0.md).
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
LIGHTER="${LIGHTER_BIN:-target/release/lighter}"
export LIGHTER_HOME="$(mktemp -d -t lighter-m15)"
D="docker -H unix://$LIGHTER_HOME/docker.sock"
ZBT=303a:831a
TR=1a86:7523
ZBT_NAME=/dev/serial/by-id/usb-Nabu_Casa_ZBT-2_E072A1D9E0CC-if00
TR_NAME=/dev/serial/by-id/usb-1a86_USB_Serial-if00-port0
FAILED=0
pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
HOLD_PID=""
cleanup() {
	[ -z "$HOLD_PID" ] || kill "$HOLD_PID" 2>/dev/null
	$D rm -f m15-zbt m15-tr >/dev/null 2>&1
	"$LIGHTER" stop >/dev/null 2>&1 || true
	mkdir -p .logs
	[ ! -f "$LIGHTER_HOME/machine.log" ] || cp "$LIGHTER_HOME/machine.log" .logs/m15-usb-last-boot.log
	rm -rf "$LIGHTER_HOME"
}
trap cleanup EXIT

present() { system_profiler SPUSBHostDataType 2>/dev/null | grep -qi "Vendor ID: 0x${1%%:*}" && ioreg -p IOUSB -l -w0 | grep -q "\"idProduct\" = $((16#${1##*:}))"; }
if ! present "$ZBT" || ! present "$TR"; then
	echo "m15-usb: skipped, the ZBT-2 and the ThirdReality dongle are not both plugged into this Mac"
	exit 0
fi
macos_port() { ls /dev/cu.usbmodemE072A1D9E0CC1 /dev/cu.usbserial-* 2>/dev/null | grep -c "${1}"; }
guest_names() { "$LIGHTER" usb ls-serial 2>/dev/null; }
wait_for() { # seconds, then a command that must succeed
	local limit="$1"; shift
	for _ in $(seq 1 $((limit * 5))); do "$@" && return 0; sleep 0.2; done
	return 1
}
has_name() { guest_names | grep -q "$1"; }
no_name() { ! guest_names | grep -q "$1"; }
macos_has() { [ -e "$1" ]; }
macos_lacks() { [ ! -e "$1" ]; }

# The radios' clients live in two containers kept for the whole gate, so a
# check pays for a probe and not for pip.
radio_containers() {
	$D rm -f m15-zbt m15-tr >/dev/null 2>&1
	$D run -d --name m15-zbt --device "$ZBT_NAME:/dev/zbt" python:3.12-alpine sleep infinity >/dev/null 2>&1 &&
		$D exec m15-zbt pip install -q universal-silabs-flasher >/dev/null 2>&1
	$D run -d --name m15-tr --device "$TR_NAME:/dev/tr" -v "$ROOT/scripts/gates/fixtures:/t:ro" python:3.12-alpine sleep infinity >/dev/null 2>&1 &&
		$D exec m15-tr pip install -q zigpy-blz >/dev/null 2>&1
}
zbt_version() {
	$D exec m15-zbt universal-silabs-flasher --device /dev/zbt --probe-methods ezsp:460800 probe 2>&1 |
		grep -oE "Detected ApplicationType.EZSP, version '[0-9.]+" | grep -oE "[0-9.]+$"
}
tr_version() {
	$D exec m15-tr python /t/blz-probe.py /dev/tr 2>/dev/null | grep -E "^stack version|^[0-9]+ version requests"
}

printf '{"usb":[{"spec":"%s"},{"spec":"%s"}]}\n' "$ZBT" "$TR" > "$LIGHTER_HOME/config.json"
echo "==> Booting with both devices in the configuration"
"$LIGHTER" start >"$LIGHTER_HOME/start.log" 2>&1 &
for _ in $(seq 1 90); do $D info >/dev/null 2>&1 && break; sleep 1; done
$D info >/dev/null 2>&1 || { fail "machine did not come up"; exit 1; }

echo "==> Attached at start"
if wait_for 30 has_name "$ZBT_NAME" && wait_for 10 has_name "$TR_NAME"; then
	pass "both devices attached, with udev's names: $(guest_names | awk '{print $1}' | xargs)"
else
	fail "names in the guest: $(guest_names | xargs)"
fi
"$LIGHTER" usb list | grep -A1 -E "$ZBT|$TR" | grep -q "attached" && pass "lighter usb list says attached" || fail "lighter usb list: $("$LIGHTER" usb list 2>&1 | tail -6 | xargs)"
if macos_lacks /dev/cu.usbmodemE072A1D9E0CC1 && ! ls /dev/cu.usbserial-* >/dev/null 2>&1; then
	pass "macOS's own ports are gone while the guest has the devices"
else
	fail "macOS still shows: $(ls /dev/cu.usb* 2>/dev/null | xargs)"
fi

echo "==> The radios answer through containers"
radio_containers
v="$(zbt_version)"
[ -n "$v" ] && pass "ZBT-2 (cdc_acm): EmberZNet $v at 460800 baud" || fail "the ZBT-2 did not answer an EZSP probe"
t="$(tr_version)"
echo "$t" | grep -q "stack version" && pass "ThirdReality (ch341, 2 Mbaud): $(echo "$t" | tr '\n' ' ')" || fail "the ThirdReality dongle did not answer over BLZ"

echo "==> Detach and attach"
$D rm -f m15-zbt >/dev/null 2>&1
"$LIGHTER" usb detach "$ZBT" >/dev/null
if wait_for 10 macos_has /dev/cu.usbmodemE072A1D9E0CC1 && wait_for 5 no_name "$ZBT_NAME"; then
	pass "detached: macOS has the ZBT-2 back and the guest does not"
else
	fail "after detach: macOS $(ls /dev/cu.usb* 2>/dev/null | xargs), guest $(guest_names | xargs)"
fi
out="$("$LIGHTER" usb attach "$ZBT" 2>&1)"
echo "$out" | grep -q "is attached" && echo "$out" | grep -q "$ZBT_NAME" && pass "attached again, and attach names the device's path" || fail "attach: $out"
$D rm -f m15-zbt >/dev/null 2>&1
# Straight back, with no pause: the device is still being given back when
# the attach arrives, and must not be seized mid-release.
for i in 1 2 3; do
	"$LIGHTER" usb detach "$ZBT" >/dev/null
	out="$("$LIGHTER" usb attach "$ZBT" 2>&1)"
	echo "$out" | grep -q "is attached" || break
done
$D run -d --name m15-zbt --device "$ZBT_NAME:/dev/zbt" python:3.12-alpine sleep infinity >/dev/null 2>&1 &&
	$D exec m15-zbt pip install -q universal-silabs-flasher >/dev/null 2>&1
echo "$out" | grep -q "is attached" && [ -n "$(zbt_version)" ] && pass "detached and attached back to back three times, and the radio answers" || fail "back-to-back detach and attach: $out"

echo "==> Unplugged on the guest's side, it comes back on its own"
python3 - "$LIGHTER_HOME" <<'PY'
import os, socket, sys
with socket.socket(socket.AF_UNIX) as c:
    c.settimeout(20); c.connect(os.path.join(sys.argv[1], 'control.sock'))
    c.sendall(b'sh for p in 0 1 2 3 4 5 6 7; do echo $p > /sys/devices/platform/vhci_hcd.0/detach 2>/dev/null; done\n')
    r = b''
    while not r.endswith(b'--end--\n'):
        d = c.recv(65536)
        if not d: break
        r += d
PY
if wait_for 5 no_name "$ZBT_NAME" && wait_for 20 has_name "$ZBT_NAME" && wait_for 20 has_name "$TR_NAME"; then
	pass "both devices were attached again after the guest dropped them"
else
	fail "after a guest-side unplug: $(guest_names | xargs)"
fi

echo "==> The agent restarts under attached devices"
python3 - "$LIGHTER_HOME" <<'PY'
import os, socket, sys
with socket.socket(socket.AF_UNIX) as c:
    c.settimeout(20); c.connect(os.path.join(sys.argv[1], 'control.sock'))
    c.sendall(b'sh for p in $(pidof lighter-agent); do tr "\\0" " " </proc/$p/cmdline | grep -q -- "--usb" && kill -9 $p; done\n')
    r = b''
    while not r.endswith(b'--end--\n'):
        d = c.recv(65536)
        if not d: break
        r += d
PY
sleep 2
has_name "$ZBT_NAME" && has_name "$TR_NAME" && pass "the names survive the agent's restart" || fail "names after the agent's restart: $(guest_names | xargs)"
"$LIGHTER" usb detach "$TR" >/dev/null
wait_for 10 macos_has /dev/cu.usbserial-210 || true
out="$("$LIGHTER" usb attach "$TR" 2>&1)"
echo "$out" | grep -q "is attached" && pass "the restarted agent attaches a device" || fail "attach after the agent's restart: $out"

echo "==> A port a Mac program holds is refused"
"$LIGHTER" usb detach "$TR" >/dev/null
wait_for 10 macos_has /dev/cu.usbserial-210
python3 -c 'import time,os; f=os.open("/dev/cu.usbserial-210", os.O_RDWR|os.O_NONBLOCK); time.sleep(60)' &
HOLD_PID=$!
sleep 1
out="$("$LIGHTER" usb attach "$TR" 2>&1)"
if echo "$out" | grep -q "is open in"; then
	pass "refused while Python holds it: $(echo "$out" | head -1)"
else
	fail "attach of a held port: $out"
fi
kill "$HOLD_PID" 2>/dev/null
HOLD_PID=""
wait_for 15 has_name "$TR_NAME" && pass "attached on its own once the port was let go" || fail "not attached after the port was let go: $(guest_names | xargs)"

echo "==> A machine started again attaches its devices again"
"$LIGHTER" stop >/dev/null 2>&1
if wait_for 10 macos_has /dev/cu.usbmodemE072A1D9E0CC1 && wait_for 10 macos_has /dev/cu.usbserial-210; then
	pass "stopped: macOS has both devices back"
else
	fail "after stop, macOS has: $(ls /dev/cu.usb* 2>/dev/null | xargs)"
fi
"$LIGHTER" start >"$LIGHTER_HOME/start.log" 2>&1 &
for _ in $(seq 1 90); do $D info >/dev/null 2>&1 && break; sleep 1; done
wait_for 30 has_name "$ZBT_NAME" && wait_for 10 has_name "$TR_NAME" && pass "started: both attached again" || fail "after start: $(guest_names | xargs)"

echo "==> A machine killed holding them gives them back"
pid="$(cat "$LIGHTER_HOME/lighter.pid" 2>/dev/null)"
kill -9 "$pid" 2>/dev/null
if wait_for 10 macos_has /dev/cu.usbmodemE072A1D9E0CC1 && wait_for 5 macos_has /dev/cu.usbserial-210; then
	pass "killed with kill -9: macOS has both devices back"
else
	fail "after the machine was killed, macOS has: $(ls /dev/cu.usb* 2>/dev/null | xargs)"
fi

echo
[ "$FAILED" -eq 0 ] && echo "m15-usb: all checks passed" || echo "m15-usb: FAILED"
exit $FAILED
