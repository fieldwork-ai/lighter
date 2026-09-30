#!/usr/bin/env bash
# Milestone 15 gate, part two: a Zigbee network through a passed-through
# stick, with a person at the Mac for what only a person can do.
#
# Runs Zigbee2MQTT on the Home Assistant Connect ZBT-2 (EmberZNet at 460800
# baud, RTS/CTS), in a lighter machine of its own that is kept between the
# phases, so the network formed in one is the network paired in the next.
# Never the daily driver: its home is ~/lighter-m15-part2.
#
#   network  — unattended: Zigbee2MQTT forms its network on the stick, the
#              coordinator answers, and a restart resumes the same network
#   pair     — a person puts the sensor and the bulb in pairing mode; both
#              join and are interviewed
#   devices  — the sensor reports a temperature; the bulb switches on and
#              off, and says so
#   replug   — a person unplugs the stick and plugs it back in; it comes
#              back, and Zigbee2MQTT with it
#   sleep    — a person sleeps and wakes the Mac; the same
#   down     — stops the machine and gives the stick back to macOS
#
#   scripts/gates/m15-usb-part2.sh <phase>... (default: network)
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
LIGHTER="$ROOT/${LIGHTER_BIN:-target/release/lighter}"
HOME2="${M15_PART2_HOME:-$HOME/lighter-m15-part2}"
export LIGHTER_HOME="$HOME2/home"
D="docker -H unix://$LIGHTER_HOME/docker.sock"
ZBT=303a:831a
ZBT_NAME=/dev/serial/by-id/usb-Nabu_Casa_ZBT-2_E072A1D9E0CC-if00
MQTT=m15p2-mqtt
Z2M=m15p2-z2m
FAILED=0

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
note() { printf '  \033[33m··\033[0m   %s\n' "$*"; }
wait_for() { # seconds, then a command that must succeed
	local n=$1; shift
	for _ in $(seq 1 "$n"); do "$@" && return 0; sleep 1; done
	return 1
}
ask() { # a step for the person at the Mac; returns when they press return
	printf '\n  \033[1m>>\033[0m  %s\n      press return when done ' "$*"
	read -r _ </dev/tty
}
has_name() { "$LIGHTER" usb ls-serial 2>/dev/null | grep -q "$ZBT_NAME"; }
running() { $D info >/dev/null 2>&1; }
# One message from a topic, retained or the next to arrive, within `wait` s.
mqtt_get() { # topic wait
	$D exec "$MQTT" mosquitto_sub -t "$1" -C 1 -W "$2" 2>/dev/null
}
mqtt_request() { # topic payload: publish, and answer with the bridge's reply
	local reply="zigbee2mqtt/bridge/response/${1#zigbee2mqtt/bridge/request/}"
	$D exec "$MQTT" sh -c "mosquitto_sub -t '$reply' -C 1 -W 30 & sleep 0.5; mosquitto_pub -t '$1' -m '$2'; wait" 2>/dev/null
}
starts() { $D logs "$Z2M" 2>&1 | grep -c "Zigbee2MQTT started"; }
z2m_started() { [ "$(starts)" -gt 0 ]; }
started_again() { [ "$(starts)" -gt "$1" ]; } # more starts than before
devices() { # the joined devices' friendly names, one per line
	mqtt_get zigbee2mqtt/bridge/devices 10 |
		python3 -c 'import json,sys; [print(d["friendly_name"], d.get("definition",{}) and d["definition"].get("model","?"), d.get("interview_completed")) for d in json.load(sys.stdin) if d["type"]!="Coordinator"]'
}

up() {
	[ -x "$LIGHTER" ] || { echo "no $LIGHTER: cargo build --release, then scripts/sign.sh it" >&2; exit 2; }
	mkdir -p "$LIGHTER_HOME" "$HOME2/z2m" "$HOME2/mosquitto"
	if ! running; then
		printf '{"usb":[{"spec":"%s"}]}\n' "$ZBT" >"$LIGHTER_HOME/config.json"
		# Outlives this script: the next phase finds the same machine.
		nohup "$LIGHTER" start >"$HOME2/start.log" 2>&1 </dev/null &
		wait_for 90 running || { fail "the machine did not come up (see $HOME2/start.log)"; exit 1; }
	fi
	wait_for 30 has_name || { fail "the ZBT-2 is not in the guest: $("$LIGHTER" usb ls-serial 2>&1 | xargs)"; exit 1; }
	printf 'listener 1883\nallow_anonymous true\n' >"$HOME2/mosquitto/mosquitto.conf"
	[ -f "$HOME2/z2m/configuration.yaml" ] || cat >"$HOME2/z2m/configuration.yaml" <<-YAML
		homeassistant:
		  enabled: false
		frontend:
		  enabled: true
		  port: 8080
		mqtt:
		  base_topic: zigbee2mqtt
		  server: mqtt://$MQTT:1883
		serial:
		  port: /dev/ttyACM0
		  adapter: ember
		  baudrate: 460800
		  rtscts: true
		advanced:
		  log_level: info
		  network_key: GENERATE
		  pan_id: GENERATE
		  ext_pan_id: GENERATE
	YAML
	$D network create m15p2 >/dev/null 2>&1 || true
	$D inspect "$MQTT" >/dev/null 2>&1 ||
		$D run -d --name "$MQTT" --network m15p2 --restart unless-stopped \
			-v "$HOME2/mosquitto/mosquitto.conf:/mosquitto/config/mosquitto.conf:ro" eclipse-mosquitto:2 >/dev/null
	$D inspect "$Z2M" >/dev/null 2>&1 ||
		$D run -d --name "$Z2M" --network m15p2 --restart unless-stopped -p 8080:8080 \
			--device "$ZBT_NAME:/dev/ttyACM0" -v "$HOME2/z2m:/app/data" koenkk/zigbee2mqtt:latest >/dev/null
	$D start "$MQTT" "$Z2M" >/dev/null 2>&1
}

phase_network() {
	echo "==> Zigbee2MQTT forms its network on the ZBT-2"
	up
	if wait_for 180 z2m_started; then
		pass "Zigbee2MQTT started on the ZBT-2 (ember, 460800 baud, RTS/CTS)"
	else
		fail "Zigbee2MQTT did not start: $($D logs --tail 15 "$Z2M" 2>&1 | tr '\n' ' ')"
		return
	fi
	local info
	info="$(mqtt_get zigbee2mqtt/bridge/info 20)"
	local coordinator
	coordinator="$(echo "$info" | python3 -c 'import json,sys; i=json.load(sys.stdin); c=i["coordinator"]; n=i["network"]; print(c["type"], c["ieee_address"], c["meta"].get("revision",""), "pan", n["pan_id"], "channel", n["channel"])' 2>/dev/null)"
	[ -n "$coordinator" ] && pass "the coordinator answers: $coordinator" || fail "no coordinator in bridge/info: ${info:0:200}"
	echo "==> A restart resumes the same network"
	local before="$coordinator" count
	count="$(starts)"
	$D restart "$Z2M" >/dev/null
	if wait_for 180 started_again "$count"; then
		info="$(mqtt_get zigbee2mqtt/bridge/info 20)"
		local after
		after="$(echo "$info" | python3 -c 'import json,sys; i=json.load(sys.stdin); c=i["coordinator"]; n=i["network"]; print(c["type"], c["ieee_address"], c["meta"].get("revision",""), "pan", n["pan_id"], "channel", n["channel"])' 2>/dev/null)"
		[ "$after" = "$before" ] && pass "restarted onto the same network" || fail "after a restart: '$after', before: '$before'"
	else
		fail "Zigbee2MQTT did not start again: $($D logs --tail 15 "$Z2M" 2>&1 | tr '\n' ' ')"
	fi
	note "the frontend: http://127.0.0.1:8080"
}

phase_pair() {
	echo "==> The sensor and the bulb join"
	up
	wait_for 180 z2m_started || { fail "Zigbee2MQTT is not running"; return; }
	mqtt_request zigbee2mqtt/bridge/request/permit_join '{"time":254}' >/dev/null
	ask "Joining is open for four minutes. Put the ThirdReality sensor and bulb in pairing mode (the bulb: power it off and on five times)."
	if wait_for 240 two_joined; then
		pass "both joined and were interviewed: $(devices | awk '{print $1 "(" $2 ")"}' | xargs)"
	else
		fail "joined so far: $(devices | xargs)"
	fi
	mqtt_request zigbee2mqtt/bridge/request/permit_join '{"time":0}' >/dev/null
}

two_joined() { [ "$(devices | grep -c ' True$')" -ge 2 ]; }
bulb() { devices | awk 'tolower($2) ~ /bulb|3rsb|3rcb/ || tolower($1) ~ /bulb/ {print $1; exit}'; }
sensor() { devices | awk 'tolower($2) ~ /th|temp|3rths|3rth/ && !(tolower($2) ~ /bulb/) {print $1; exit}'; }

phase_devices() {
	echo "==> The devices work"
	up
	wait_for 180 z2m_started || { fail "Zigbee2MQTT is not running"; return; }
	local s b
	s="$(sensor)"
	b="$(bulb)"
	if [ -n "$s" ]; then
		ask "Breathe on the sensor, or hold it for a moment, so it reports."
		local t
		t="$(mqtt_get "zigbee2mqtt/$s" 120 | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("temperature"), "°C,", d.get("humidity"), "% RH")' 2>/dev/null)"
		[ -n "$t" ] && pass "the sensor ($s) reports: $t" || fail "no report from the sensor ($s) in two minutes"
	else
		fail "no sensor among the devices: $(devices | xargs)"
	fi
	if [ -n "$b" ]; then
		local ok=1
		for state in ON OFF ON; do
			$D exec "$MQTT" mosquitto_pub -t "zigbee2mqtt/$b/set" -m "{\"state\":\"$state\"}"
			local got
			got="$(mqtt_get "zigbee2mqtt/$b" 15 | python3 -c 'import json,sys; print(json.load(sys.stdin).get("state"))' 2>/dev/null)"
			[ "$got" = "$state" ] || { ok=0; fail "the bulb ($b) was told $state and says '$got'"; }
		done
		[ "$ok" = 1 ] && ask "Did the bulb go on, off and on? (If not, say so in the review.)" && pass "the bulb ($b) switched on, off and on, and said so each time"
	else
		fail "no bulb among the devices: $(devices | xargs)"
	fi
}

phase_replug() {
	echo "==> Unplugged by hand and plugged back in"
	up
	ask "Unplug the ZBT-2, wait five seconds, and plug it back in."
	if wait_for 60 has_name; then
		pass "the ZBT-2 is back in the guest under its name"
	else
		fail "the ZBT-2 did not come back: $("$LIGHTER" status 2>&1 | grep usb | xargs)"
		return
	fi
	# The container's device node went with the old device: restart it onto
	# the new one, as a Linux host's would need.
	local count
	count="$(starts)"
	$D restart "$Z2M" >/dev/null
	wait_for 180 started_again "$count" &&
		pass "Zigbee2MQTT resumed its network on the replugged stick" || fail "Zigbee2MQTT after the replug: $($D logs --tail 10 "$Z2M" 2>&1 | tr '\n' ' ')"
}

phase_sleep() {
	echo "==> The Mac sleeps and wakes"
	up
	ask "Sleep the Mac (Apple menu > Sleep), wait a minute, and wake it."
	wait_for 60 has_name && pass "the ZBT-2 is in the guest after the wake" || fail "after the wake: $("$LIGHTER" status 2>&1 | grep usb | xargs)"
	mqtt_request zigbee2mqtt/bridge/request/health_check '' | grep -q '"healthy":true' &&
		pass "Zigbee2MQTT is healthy after the wake" || fail "Zigbee2MQTT's health check after the wake: $($D logs --tail 10 "$Z2M" 2>&1 | tr '\n' ' ')"
}

phase_down() {
	echo "==> Down"
	"$LIGHTER" stop >/dev/null 2>&1 && pass "stopped; the ZBT-2 is macOS's again" || note "not running"
}

[ $# -gt 0 ] || set -- network
for phase in "$@"; do
	case "$phase" in
	network | pair | devices | replug | sleep | down) "phase_$phase" ;;
	*) echo "unknown phase: $phase" >&2; exit 2 ;;
	esac
done
echo
[ "$FAILED" = 0 ] && echo "m15-usb part two ($*): all checks passed" || { echo "m15-usb part two ($*): FAILED"; exit 1; }
