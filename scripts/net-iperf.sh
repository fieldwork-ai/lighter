#!/usr/bin/env bash
# The network iteration loop, and the load gate.
#
#   scripts/net-iperf.sh                 # four paths, 2 s each, then the checks
#   scripts/net-iperf.sh --seconds 20    # the gate's length
#   scripts/net-iperf.sh --path egress   # one path while working on it
#   scripts/net-iperf.sh --soak 600      # alternate directions for ten minutes
#
# Boots a throwaway machine on the real CLI under its own LIGHTER_HOME (never
# the daily driver), measures iperf3 on four paths, and then asks the two
# questions that a throughput number does not answer: did the guest log an
# RCU stall, and does the Docker socket still answer. Either is a red exit,
# because the first version of the device blocked a vCPU thread on the
# network socket under load, which made both true at 1.5 Gbit/s.
#
# The paths, from the container's point of view:
#   egress    container -> the Mac's LAN address     (iperf3 -c <lan-ip>)
#   egress-r  the Mac -> container, same connection  (iperf3 -R)
#   port      the Mac -> a published port            (iperf3 -c 127.0.0.1 -p)
#   port-r    the container -> the Mac, same port    (iperf3 -R)
#
# Output is one `path=<name> gbits=<n>` line per path and a final
# `stall=<0|1> docker_ps_ms=<n> result=<ok|FAIL>` line.
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SECONDS_PER_PATH=2
ONLY_PATH=""
SOAK=0
LIGHTER_BIN="${LIGHTER_BIN:-target/release/lighter}"
IMAGE=networkstatic/iperf3
IMAGE_TAR=".logs/iperf3.tar"
HOST_PORT=5299
PUBLISHED_PORT=5298
while [ $# -gt 0 ]; do
	case "$1" in
	--seconds) SECONDS_PER_PATH="$2"; shift 2 ;;
	--path) ONLY_PATH="$2"; shift 2 ;;
	--soak) SOAK="$2"; shift 2 ;;
	*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
done

command -v iperf3 >/dev/null || { echo "iperf3 is required on the host (brew install iperf3)" >&2; exit 2; }
[ -x "$LIGHTER_BIN" ] || { echo "no $LIGHTER_BIN; cargo build --release -p lighter-cli && scripts/sign.sh $LIGHTER_BIN" >&2; exit 2; }
LAN_IP="$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null)"
[ -n "$LAN_IP" ] || { echo "no LAN address on en0/en1" >&2; exit 2; }

export LIGHTER_HOME="$(mktemp -d -t lighter-net)"
DOCKER="docker -H unix://$LIGHTER_HOME/docker.sock"
FAILED=0
cleanup() {
	pkill -f "iperf3 -s -D -p $HOST_PORT" 2>/dev/null || true
	"$LIGHTER_BIN" stop >/dev/null 2>&1 || true
	# A machine that will not stop (the failure this script exists to catch)
	# is killed by pid, so the next run does not inherit it.
	[ -f "$LIGHTER_HOME/lighter.pid" ] && kill -9 "$(cat "$LIGHTER_HOME/lighter.pid")" 2>/dev/null || true
	pkill -f "gvproxy.*$LIGHTER_HOME" 2>/dev/null || true
	rm -rf "$LIGHTER_HOME"
}
trap cleanup EXIT

# Boot, and wait for Docker rather than for the process.
"$LIGHTER_BIN" start >"$LIGHTER_HOME/start.log" 2>&1 &
for _ in $(seq 1 60); do
	$DOCKER info >/dev/null 2>&1 && break
	sleep 1
done
if ! $DOCKER info >/dev/null 2>&1; then
	echo "machine did not come up: $(tail -3 "$LIGHTER_HOME/start.log" | tr '\n' ' ')" >&2
	exit 1
fi

# The image from a local tarball when there is one: a pull measures the
# internet, and every throwaway machine starts with an empty disk.
if [ -f "$IMAGE_TAR" ]; then
	$DOCKER load -i "$IMAGE_TAR" >/dev/null 2>&1
else
	$DOCKER pull -q "$IMAGE" >/dev/null 2>&1
fi

iperf3 -s -D -p "$HOST_PORT" --logfile "$LIGHTER_HOME/iperf-host.log"
$DOCKER run -d --rm --name iperf-port -p "$PUBLISHED_PORT:5201" "$IMAGE" -s >/dev/null 2>&1
sleep 1

# The receiver's figure, in Gbit/s; "0" when iperf3 printed nothing (a stalled
# guest prints nothing, and the checks below say why).
receiver_gbits() { grep receiver | awk '{ if ($8 == "Mbits/sec") printf "%.2f\n", $7 / 1000; else if ($8 == "Gbits/sec") printf "%.2f\n", $7; else print "0" }' | head -1; }
measure() {
	local path="$1" secs="$2" out
	# Each measurement under its own cap: a guest that stalls mid-transfer
	# hangs the client, and the point is to record that as a zero and go on
	# to the checks that name it, not to hang the loop with it.
	local cap=$(( secs + 25 ))
	case "$path" in
	egress)   out="$(scripts/capped.sh "$cap" $DOCKER run --rm "$IMAGE" -c "$LAN_IP" -p "$HOST_PORT" -t "$secs" 2>&1)" ;;
	egress-r) out="$(scripts/capped.sh "$cap" $DOCKER run --rm "$IMAGE" -c "$LAN_IP" -p "$HOST_PORT" -t "$secs" -R 2>&1)" ;;
	port)     out="$(scripts/capped.sh "$cap" iperf3 -c 127.0.0.1 -p "$PUBLISHED_PORT" -t "$secs" 2>&1)" ;;
	port-r)   out="$(scripts/capped.sh "$cap" iperf3 -c 127.0.0.1 -p "$PUBLISHED_PORT" -t "$secs" -R 2>&1)" ;;
	esac
	echo "path=$path gbits=$(echo "$out" | receiver_gbits | grep . || echo 0)"
}

if [ "$SOAK" -gt 0 ]; then
	end=$(( $(date +%s) + SOAK ))
	while [ "$(date +%s)" -lt "$end" ]; do
		for p in egress egress-r port port-r; do measure "$p" 5; done
	done
else
	for p in egress egress-r port port-r; do
		[ -n "$ONLY_PATH" ] && [ "$ONLY_PATH" != "$p" ] && continue
		measure "$p" "$SECONDS_PER_PATH"
	done
fi

# The two questions the number does not answer.
stall=0
grep -q "rcu_sched detected stalls\|rcu: INFO\|soft lockup\|hung task" "$LIGHTER_HOME/machine.log" 2>/dev/null && stall=1
t0=$(python3 -c 'import time; print(int(time.time()*1000))')
if $DOCKER ps >/dev/null 2>&1 & pid=$!; then
	for _ in $(seq 1 50); do kill -0 $pid 2>/dev/null || break; sleep 0.1; done
	if kill -0 $pid 2>/dev/null; then kill -9 $pid 2>/dev/null; docker_ms=5000; else docker_ms=$(( $(python3 -c 'import time; print(int(time.time()*1000))') - t0 )); fi
fi
result=ok
{ [ "$stall" = 1 ] || [ "$docker_ms" -ge 5000 ]; } && { result=FAIL; FAILED=1; }
echo "stall=$stall docker_ps_ms=$docker_ms result=$result"
[ "$stall" = 1 ] && cp "$LIGHTER_HOME/machine.log" ".logs/net-stall-$(date +%s).log" 2>/dev/null
exit $FAILED
