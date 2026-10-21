#!/usr/bin/env bash
# Milestone 21 gate: a folder shared from the Mac behaves like a local disk.
#
# What a container has to be able to rely on there:
#
#   1. a file it has open stays the file it opened. Deleted on the Mac, saved
#      over on the Mac (a temporary renamed into place), moved there, or
#      deleted, replaced or used as scratch in the container, it reads what
#      it opened; an edit made in place on the Mac is read at once
#   2. a Mac-side delete moments after the open finds the file already held:
#      the guest says what it has open in batches of up to a tick (guest
#      patch 0057), so one inside the first few milliseconds may miss
#   3. fsx, with every operation it has, three seeds, mapped and not: no
#      read ever disagrees with what was written
#   4. pjdfstest fails nothing it did not fail when the known list was taken
#      (fixtures/m21/pjdfstest-known.txt). What that list holds is the share's
#      deliberate model, as on Docker Desktop: a Mac file belongs to whoever
#      asks, so a check meant to refuse another user passes, and the test's
#      later steps follow from that. A subtest that starts passing is named,
#      to be taken off the list
#
# The suites run as root in the container, which is what they are written
# for. About fifteen minutes, most of it pjdfstest.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

PROFILE="${PROFILE:-release}"
LIGHTER="${LIGHTER_BIN:-target/$PROFILE/lighter}"
FIXTURES="$ROOT/scripts/gates/fixtures/m21"
FAILED=0
pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
note() { printf '  \033[33m··\033[0m   %s\n' "$*"; }

export LIGHTER_HOME="$(mktemp -d -t lighter-m21-home)"
export DOCKER_HOST="unix://$LIGHTER_HOME/docker.sock"
# Under /Users, which the machine shares at the same path.
SCRATCH="$HOME/.lighter-gate-m21-$$"
mkdir -p "$SCRATCH"
cleanup() {
	docker rm -f m21-probe >/dev/null 2>&1 || true
	"$LIGHTER" stop >/dev/null 2>&1 || true
	[ "$FAILED" = 0 ] || { mkdir -p "$ROOT/.logs"; cp "$LIGHTER_HOME/machine.log" "$ROOT/.logs/m21-machine-$(date +%H%M%S).log" 2>/dev/null; cp "$SCRATCH/pjd.log" "$ROOT/.logs/m21-pjdfstest-$(date +%H%M%S).log" 2>/dev/null; } || true
	rm -rf "$LIGHTER_HOME" "$SCRATCH"
}
trap cleanup EXIT

PYTHON="python:3.13-alpine"
CONFORM="lighter-m21-conform"

"$LIGHTER" start >/dev/null 2>&1 || { fail "the machine did not start"; exit 1; }
docker pull -q "$PYTHON" >/dev/null || { fail "could not pull $PYTHON"; exit 1; }
docker build -q -t "$CONFORM" "$FIXTURES" >/dev/null || { fail "could not build the conformance image"; exit 1; }

echo "== 1. a file it has open stays the file it opened"
P="$SCRATCH/probes"
mkdir -p "$P/elsewhere"
for c in host-delete-unread host-delete-read host-replace-unread host-replace-read host-rename-unread host-inplace-read; do
	mkdir -p "$P/$c" && echo old-contents > "$P/$c/f"
done
cp "$FIXTURES/probe.py" "$P/"
docker run -d --rm --name m21-probe -v "$P:/m" "$PYTHON" python3 /m/probe.py >/dev/null
for _ in $(seq 600); do [ -e "$P/ready" ] && break; sleep 0.05; done
if [ -e "$P/ready" ]; then
	# The Mac acts while the container holds each file open.
	rm "$P/host-delete-unread/f" "$P/host-delete-read/f"
	for c in host-replace-unread host-replace-read; do echo new-contents > "$P/$c/f.tmp"; mv "$P/$c/f.tmp" "$P/$c/f"; done
	mv "$P/host-rename-unread/f" "$P/elsewhere/f"
	printf 'new-contents\n' | dd of="$P/host-inplace-read/f" conv=notrunc 2>/dev/null
	touch "$P/go"
	for _ in $(seq 600); do [ -e "$P/result.json" ] && break; sleep 0.05; done
fi
if [ -e "$P/result.json" ]; then
	while read -r verdict line; do
		if [ "$verdict" = ok ]; then pass "$line"; else fail "$line"; fi
	done < <(python3 - "$P/result.json" <<'EOF'
import json, sys
r = json.load(open(sys.argv[1]))
want = {
    "host-delete-unread": ("old", "deleted on the Mac, open and unread"),
    "host-delete-read": ("old", "deleted on the Mac, open and read"),
    "host-replace-unread": ("old", "saved over on the Mac, open and unread"),
    "host-replace-read": ("old", "saved over on the Mac, open and read"),
    "host-rename-unread": ("old", "moved on the Mac, open"),
    "host-inplace-read": ("new", "edited in place on the Mac, open: the edit is read at once"),
    "guest-delete-unread": ("old", "deleted in the container, open"),
    "guest-replace-unread": ("old", "replaced in the container, open"),
    "guest-tmpfile": ("same", "opened, unlinked and used as scratch in the container"),
}
show = lambda v: {"old-contents": "old", "new-contents": "new"}.get(v.strip(), v.strip())
for case, (w, what) in want.items():
    got = r[case]
    now, later = show(got["now"]), show(got.get("later", got["now"]))
    verdict = "ok" if now == w and later == w else "bad"
    detail = "" if verdict == "ok" else f" (read {now}, then {later})"
    print(verdict, what + detail)
EOF
)
else
	fail "the probe container never reported"
	docker logs m21-probe 2>&1 | tail -5
fi
docker rm -f m21-probe >/dev/null 2>&1

echo "== 2. a delete on the Mac moments after the open"
R="$SCRATCH/race"
mkdir -p "$R/race"
for w in 0 1 2 3 4 6 8 12; do for i in $(seq 0 9); do echo old > "$R/race/f$w-$i"; done; done
cp "$FIXTURES/race.py" "$R/"
( for w in 0 1 2 3 4 6 8 12; do for i in $(seq 0 9); do
	p="$R/race/f$w-$i"; until [ -e "$p.go" ]; do :; done; rm "$p"; touch "$p.done"
done; done ) &
deleter=$!
held="$(docker run --rm -v "$R:/m" "$PYTHON" python3 /m/race.py)"
wait "$deleter"
missed="$(python3 -c "
import sys
counts = dict(part.split(':') for part in sys.argv[1].split())
print(sum(10 - int(v.split('/')[0]) for v in counts.values()))" "$held")"
late="$(python3 -c "
import sys
counts = dict(part.split(':') for part in sys.argv[1].split())
print(sum(10 - int(v.split('/')[0]) for k, v in counts.items() if int(k.rstrip('ms')) >= 6))" "$held")"
if [ "$late" = 0 ] && [ "$missed" -le 2 ]; then
	pass "held at 0 to 12 ms after the open: $held"
else
	fail "a file deleted on the Mac after its open was not held: $held"
fi

echo "== 3. fsx, every operation, three seeds"
F="$SCRATCH/fsx"
mkdir -p "$F"
for seed in 1 2 3; do
	for style in mapped read-write; do
		args=(-q -N 100000 -S "$seed" -P /m)
		[ "$style" = read-write ] && args+=(-R -W)
		if docker run --rm -v "$F:/m" "$CONFORM" timeout 1500 /opt/xfstests/ltp/fsx "${args[@]}" "/m/$style-$seed" > "$F/$style-$seed.log" 2>&1; then
			pass "fsx seed $seed, $style: 100,000 operations, every read as written"
		else
			fail "fsx seed $seed, $style: $(grep -m1 -iE 'error|bad|fail|not supported|not permitted' "$F/$style-$seed.log")"
		fi
	done
done

echo "== 4. pjdfstest, against the known list"
mkdir -p "$SCRATCH/pjd"
docker run --rm -v "$SCRATCH:/m" -w /m/pjd "$CONFORM" prove -rv /opt/pjdfstest/tests > "$SCRATCH/pjd.log" 2>&1
python3 "$FIXTURES/compare.py" "$SCRATCH/pjd.log" "$FIXTURES/pjdfstest-known.txt" > "$SCRATCH/pjd-compare.txt"
case $? in
	0) pass "$(head -1 "$SCRATCH/pjd-compare.txt")" ;;
	*) fail "$(head -1 "$SCRATCH/pjd-compare.txt")"; sed -n '2,40p' "$SCRATCH/pjd-compare.txt" | sed 's/^/       /' ;;
esac
grep -q "^fixed" "$SCRATCH/pjd-compare.txt" && grep "^fixed" "$SCRATCH/pjd-compare.txt" | head -20 | while read -r line; do note "$line"; done

if [ "$FAILED" = 0 ]; then
	echo "m21: shared folders behave like a local disk"
else
	echo "m21: FAILED" >&2
fi
exit "$FAILED"
