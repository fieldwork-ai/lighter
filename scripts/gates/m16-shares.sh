#!/usr/bin/env bash
# Milestone 16 gate: folders and drives on the Mac, at the paths they have
# there.
#
# The machine shares /Users, /Volumes and /var/folders. What that has to mean
# for a person with an external drive:
#
#   1. a drive connected before the machine starts is there
#   2. one connected while it runs is there too, without a restart, whatever
#      it is formatted as (APFS, and exFAT as most drives are sold)
#   3. a change made on the Mac reaches a container that already has it open
#   4. ejecting it works while the machine runs: a machine that held it busy
#      would make every drive impossible to unplug cleanly
#   5. a bind from a folder that is not shared, or from /tmp, is named by
#      `lighter status` and `lighter doctor` instead of silently being empty
#   6. sharing all of it costs nothing while idle: /var/folders is where every
#      app on the Mac keeps its temporary files, and each change there is an
#      event the machine is told about
#
# Drives are disk images: they mount under /Volumes like any drive, and need
# neither hardware nor root.
set -euo pipefail

if ! command -v cargo >/dev/null 2>&1; then
	# shellcheck disable=SC1091
	. "$HOME/.cargo/env"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

PROFILE="${PROFILE:-release}"
LIGHTER="target/$PROFILE/lighter"
IMAGE="alpine:3.21"
# How much more CPU the default shares may cost than the home folder alone,
# in seconds over the idle window. A tenth of a second in 30 is 0.3% of a
# core, well under anything a person would see in Activity Monitor.
IDLE_WINDOW=30
IDLE_MARGIN=0.1

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
note() { printf '  \033[33m··\033[0m   %s\n' "$*"; }
FAILED=0

for tool in docker hdiutil; do
	command -v "$tool" >/dev/null 2>&1 || { echo "$tool is required" >&2; exit 1; }
done

# Its own machine in its own home, as in m8: nothing here may touch a
# daily-driver lighter running beside it.
export LIGHTER_HOME="$(mktemp -d -t lighter-m16-home)"
export DOCKER_HOST="unix://$LIGHTER_HOME/docker.sock"
SCRATCH="$(mktemp -d -t lighter-m16)"
# exFAT labels are at most 11 characters.
APFS="LGAPFS$$"
EXFAT="LGEXF$$"
UNSHARED="/opt/lighter-gate-unshared-$$"

detach() {
	[ -d "/Volumes/$1" ] && hdiutil detach -quiet "/Volumes/$1" 2>/dev/null \
		|| { [ -d "/Volumes/$1" ] && hdiutil detach -quiet -force "/Volumes/$1" 2>/dev/null; } \
		|| true
}

cleanup() {
	[ "$FAILED" = 0 ] || cp "$LIGHTER_HOME/machine.log" "$ROOT/.logs/m16-machine.log" 2>/dev/null || true
	docker rm -f lighter-gate-reader lighter-gate-unshared lighter-gate-tmp >/dev/null 2>&1 || true
	"$LIGHTER" stop >/dev/null 2>&1 || true
	detach "$APFS"
	detach "$EXFAT"
	rm -rf "$LIGHTER_HOME" "$SCRATCH"
}
trap cleanup EXIT

echo "==> Building and signing the CLI"
cargo build $([ "$PROFILE" = release ] && echo --release) -p lighter-cli
./scripts/sign.sh "$LIGHTER" >/dev/null

echo
echo "==> Two drives: APFS, and exFAT as most drives are sold"
hdiutil create -quiet -size 64m -fs APFS -volname "$APFS" "$SCRATCH/apfs.dmg"
hdiutil create -quiet -size 64m -fs ExFAT -volname "$EXFAT" "$SCRATCH/exfat.dmg"
hdiutil attach -quiet "$SCRATCH/apfs.dmg"
echo "before" > "/Volumes/$APFS/hello.txt"
pass "attached /Volumes/$APFS before the machine starts"

# The CPU time of the machine's process, in seconds.
cpu_seconds() {
	local pid
	pid="$("$LIGHTER" status | awk '$1 == "pid" { print $2 }')"
	ps -o cputime= -p "$pid" | awk -F'[:.]' '{
		n = NF; frac = $n; sec = $(n-1); min = $(n-2); hr = (n > 3) ? $(n-3) : 0
		printf "%.2f\n", hr*3600 + min*60 + sec + frac/100 }'
}

# What the machine spends doing nothing in IDLE_WINDOW seconds, once it has
# settled: the least of three windows a third as long, scaled up. Whatever
# else the Mac does only ever adds to a window, so the least is the closest
# to the machine's own cost; one window of the same length varied by more
# than the margin between two runs of the same configuration.
idle_cost() {
	docker run --rm "$IMAGE" true >/dev/null
	sleep 10
	local least="" before after spent window=$(( IDLE_WINDOW / 3 ))
	for _ in 1 2 3; do
		before="$(cpu_seconds)"
		sleep "$window"
		after="$(cpu_seconds)"
		spent="$(echo "$after - $before" | bc)"
		if [ -z "$least" ] || [ "$(echo "$spent < $least" | bc)" = 1 ]; then
			least="$spent"
		fi
	done
	echo "$least * 3" | bc
}

echo
echo "==> Idle, sharing the home folder alone"
"$LIGHTER" config --unshare /Users --unshare /Volumes --unshare /var/folders \
	--share "$HOME" >/dev/null
if "$LIGHTER" config | grep -q "share      /Volumes"; then
	fail "the home folder chosen alone was turned back into the defaults"
fi
"$LIGHTER" start >/dev/null 2>&1 || { fail "lighter start failed"; "$LIGHTER" logs | tail -15; exit 1; }
docker pull -q "$IMAGE" >/dev/null
home_only="$(idle_cost)"
note "home folder alone: ${home_only}s of CPU in ${IDLE_WINDOW}s"
"$LIGHTER" stop >/dev/null

echo
echo "==> The defaults"
# A config from before the defaults shares the home folder alone, and reads
# as them: the upgrade every existing install takes.
printf '{"shares": ["%s"]}' "$HOME" > "$LIGHTER_HOME/config.json"
if "$LIGHTER" config | grep -q "share      /Volumes"; then
	pass "a config from before 0.11.6 reads as the defaults"
else
	fail "the home folder did not become the defaults: $("$LIGHTER" config | grep share | tr '\n' ' ')"
fi
"$LIGHTER" start >/dev/null 2>&1 || { fail "lighter start failed"; "$LIGHTER" logs | tail -15; exit 1; }
defaults="$(idle_cost)"
if [ "$(echo "$defaults <= $home_only + $IDLE_MARGIN" | bc)" = 1 ]; then
	pass "the defaults cost nothing more idle (${defaults}s against ${home_only}s in ${IDLE_WINDOW}s)"
else
	fail "the defaults cost ${defaults}s of CPU idle in ${IDLE_WINDOW}s, against ${home_only}s for the home folder"
fi

echo
echo "==> A drive connected before the start"
seen="$(docker run --rm -v "/Volumes/$APFS:/d" "$IMAGE" cat /d/hello.txt 2>&1 || true)"
[ "$seen" = before ] && pass "a container reads the APFS drive" || fail "the APFS drive reads as: ${seen:-nothing}"

echo
echo "==> A drive connected while the machine runs"
hdiutil attach -quiet "$SCRATCH/exfat.dmg"
echo "plugged in" > "/Volumes/$EXFAT/hello.txt"
seen="$(docker run --rm -v "/Volumes/$EXFAT:/d" "$IMAGE" cat /d/hello.txt 2>&1 || true)"
[ "$seen" = "plugged in" ] && pass "a container reads the exFAT drive attached after the start" \
	|| fail "the exFAT drive reads as: ${seen:-nothing}"
docker run --rm -v "/Volumes/$EXFAT:/d" "$IMAGE" sh -c 'echo "from a container" > /d/out.txt' >/dev/null 2>&1 || true
seen="$(cat "/Volumes/$EXFAT/out.txt" 2>/dev/null || true)"
[ "$seen" = "from a container" ] && pass "a container writes to it, and the Mac reads what it wrote" \
	|| fail "the Mac reads the container's file as: ${seen:-nothing}"

echo
echo "==> A change on the Mac, seen by a container that has the drive open"
for volume in "$APFS" "$EXFAT"; do
	docker run -d --name lighter-gate-reader -v "/Volumes/$volume:/d" "$IMAGE" sleep 600 >/dev/null
	docker exec lighter-gate-reader cat /d/hello.txt >/dev/null
	echo "edited on the Mac" > "/Volumes/$volume/hello.txt"
	seen=""
	for _ in $(seq 1 20); do
		seen="$(docker exec lighter-gate-reader cat /d/hello.txt 2>/dev/null || true)"
		[ "$seen" = "edited on the Mac" ] && break
		sleep 0.25
	done
	[ "$seen" = "edited on the Mac" ] && pass "$volume: the container sees the edit" \
		|| fail "$volume: the container still reads: ${seen:-nothing}"
	docker rm -f lighter-gate-reader >/dev/null
done

echo
echo "==> Ejecting while the machine runs"
# No -force: that is what Finder's eject does, and it fails if anything on
# the Mac holds a file or folder on the volume open.
# What the machine holds on a volume, for a failure to name.
holding() {
	lsof -p "$("$LIGHTER" status | awk '$1 == "pid" { print $2 }')" 2>/dev/null \
		| grep "/Volumes/$1" | sed 's/^/    /'
}
if hdiutil detach "/Volumes/$EXFAT" >"$SCRATCH/detach.err" 2>&1; then
	pass "the exFAT drive ejects while the machine runs"
else
	fail "the exFAT drive could not be ejected: $(grep -v deprecated "$SCRATCH/detach.err")"
	holding "$EXFAT"
fi
if docker run --rm -v /Volumes:/v "$IMAGE" ls /v 2>/dev/null | grep -qx "$EXFAT"; then
	fail "the guest still lists the ejected drive"
else
	pass "the guest no longer lists it"
fi
seen="$(docker run --rm -v "/Volumes/$APFS:/d" "$IMAGE" cat /d/hello.txt 2>&1 || true)"
[ "$seen" = "edited on the Mac" ] && pass "the other drive is untouched" \
	|| fail "after the eject the APFS drive reads as: ${seen:-nothing}"
hdiutil attach -quiet "$SCRATCH/exfat.dmg"
seen="$(docker run --rm -v "/Volumes/$EXFAT:/d" "$IMAGE" cat /d/out.txt 2>&1 || true)"
[ "$seen" = "from a container" ] && pass "connected again, it is there again" \
	|| fail "reconnected, the exFAT drive reads as: ${seen:-nothing}"
if hdiutil detach "/Volumes/$APFS" >"$SCRATCH/detach.err" 2>&1; then
	pass "the APFS drive ejects too, after containers have used it"
else
	fail "the APFS drive could not be ejected: $(grep -v deprecated "$SCRATCH/detach.err")"
	holding "$APFS"
fi

echo
echo "==> Binds the machine cannot serve"
docker run -d --name lighter-gate-unshared -v "$UNSHARED:/data" "$IMAGE" sleep 600 >/dev/null
docker run -d --name lighter-gate-tmp -v /tmp/lighter-gate-$$:/out "$IMAGE" sleep 600 >/dev/null
status="$("$LIGHTER" status 2>&1)"
if grep -q "lighter-gate-unshared binds $UNSHARED, which is not shared: share it with \`lighter config --share $UNSHARED\`" <<<"$status"; then
	pass "lighter status names the unshared bind and how to share it"
else
	fail "lighter status does not name $UNSHARED"
	sed 's/^/    /' <<<"$status"
fi
if grep -q "lighter-gate-tmp binds /tmp/lighter-gate-$$, .*\$TMPDIR" <<<"$status"; then
	pass "lighter status says /tmp is the machine's own"
else
	fail "lighter status does not name the /tmp bind"
fi
if "$LIGHTER" status | grep -q "lighter-gate-reader\|/Volumes/$EXFAT"; then
	fail "lighter status names a bind that is shared"
fi
doctor="$("$LIGHTER" doctor 2>&1 || true)"
if grep -q "bind mounts" <<<"$doctor" && grep -q "$UNSHARED" <<<"$doctor"; then
	pass "lighter doctor warns about them"
else
	fail "lighter doctor does not mention the unshared binds"
	sed 's/^/    /' <<<"$doctor"
fi
docker rm -f lighter-gate-unshared lighter-gate-tmp >/dev/null
if "$LIGHTER" doctor 2>&1 | grep -q "bind mounts.*every one from the Mac is shared"; then
	pass "and stops once the containers have gone"
else
	fail "lighter doctor still warns with the containers gone"
fi

echo
echo "==> A shared drive that is not there"
"$LIGHTER" stop >/dev/null
MISSING="/Library/lighter-gate-missing-$$"
"$LIGHTER" config --share "$MISSING" >/dev/null 2>&1 \
	&& fail "lighter config shared a folder that is not there" \
	|| pass "lighter config refuses to share a folder that is not there"
# Written by hand, as a config naming a drive since unplugged reads.
python3 - "$LIGHTER_HOME/config.json" "$MISSING" <<'PY'
import json, sys
path, missing = sys.argv[1], sys.argv[2]
with open(path) as f:
    config = json.load(f)
config["shares"].append(missing)
with open(path, "w") as f:
    json.dump(config, f)
PY
if "$LIGHTER" start >/dev/null 2>&1 && docker version >/dev/null 2>&1; then
	pass "the machine starts with a share that is not there"
else
	fail "the machine did not start with a share that is not there"
	"$LIGHTER" logs | tail -15 | sed 's/^/    /'
fi
if "$LIGHTER" doctor 2>&1 | grep -q "not there, so not shared: $MISSING"; then
	pass "lighter doctor names it"
else
	fail "lighter doctor does not name the missing share"
fi
"$LIGHTER" config --unshare "$MISSING" >/dev/null \
	&& pass "lighter config --unshare removes it" \
	|| fail "lighter config --unshare could not remove it"

if [ "$FAILED" = 0 ]; then
	echo
	echo "m16: shared folders and drives work"
fi
exit "$FAILED"
