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
# Under pipefail `cmd | grep -q` fails whenever grep stops reading before cmd
# has finished writing, so checks grep captured output instead.

if ! command -v cargo >/dev/null 2>&1; then
	# shellcheck disable=SC1091
	. "$HOME/.cargo/env"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

PROFILE="${PROFILE:-release}"
LIGHTER="${LIGHTER_BIN:-target/$PROFILE/lighter}"
IMAGE="alpine:3.21"
PYTHON="python:3.12-slim"
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

SMB_D="$HOME/.lighter-gate-smb-d-$$"
SMB_T="$HOME/.lighter-gate-smb-t-$$"

cleanup() {
	# By the exit status too: judge records a failure in its pipeline's
	# subshell, where FAILED does not reach this.
	local status=$?
	{ [ "$FAILED" = 0 ] && [ "$status" = 0 ]; } || cp "$LIGHTER_HOME/machine.log" "$ROOT/.logs/m16-machine.log" 2>/dev/null || true
	# Unpaused first: unmounting a share whose server is paused hangs.
	docker unpause lighter-gate-smb >/dev/null 2>&1 || true
	umount "$SMB_D" 2>/dev/null || true
	umount "$SMB_T" 2>/dev/null || true
	rmdir "$SMB_D" "$SMB_T" 2>/dev/null || true
	docker rm -f lighter-gate-nested lighter-gate-reader lighter-gate-unshared lighter-gate-tmp lighter-gate-smb lighter-gate-smb-reader lighter-gate-ticker lighter-gate-local >/dev/null 2>&1 || true
	"$LIGHTER" stop >/dev/null 2>&1 || true
	detach "$APFS"
	detach "$EXFAT"
	rm -rf "$LIGHTER_HOME" "$SCRATCH"
}
trap cleanup EXIT

# LIGHTER_BIN runs a built lighter (a release) as it is: re-signing one would
# replace its signature.
if [ -z "${LIGHTER_BIN:-}" ]; then
	echo "==> Building and signing the CLI"
	cargo build $([ "$PROFILE" = release ] && echo --release) -p lighter-cli
	./scripts/sign.sh "$LIGHTER" >/dev/null
fi

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
if grep -q "share      /Volumes" <<<"$("$LIGHTER" config)"; then
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
if grep -q "share      /Volumes" <<<"$("$LIGHTER" config)"; then
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

# What a metadata-preserving move does, as Sonarr's import does it: extended
# attributes set, read and listed, then the file moved to another volume
# keeping them. Each line is "<check> ok|FAIL <detail>".
metadata() {
	docker run --rm -v "$1:/from" -v "$2:/to" "$PYTHON" python3 -c '
import os, shutil
p = "/from/meta.txt"
open(p, "w").write("x")
checks = [
    ("set an extended attribute", lambda: os.setxattr(p, "user.gate", b"1")),
    ("read it", lambda: os.getxattr(p, "user.gate") == b"1" or 1 / 0),
    ("list it", lambda: "user.gate" in os.listxattr(p) or 1 / 0),
    ("move it to another volume", lambda: shutil.move(p, "/to/meta.txt")),
    ("the moved file keeps it", lambda: os.getxattr("/to/meta.txt", "user.gate") == b"1" or 1 / 0),
]
for name, check in checks:
    try:
        check()
        print(name, "ok")
    except Exception as e:
        print(name, "FAIL", e)
os.remove("/to/meta.txt")
' 2>&1
}

# Turns metadata()'s lines into gate lines, prefixed with what was tested.
judge() {
	local what="$1" line
	while IFS= read -r line; do
		case "$line" in
		*" ok") pass "$what: ${line% ok}" ;;
		*" FAIL "*) fail "$what: ${line%% FAIL *} — ${line#* FAIL }" ;;
		# Anything else is docker's or Python's own error: shown, not dropped.
		*) [ -n "$line" ] && fail "$what: $line" ;;
		esac
	done
}

echo
echo "==> Metadata on a drive with no identity paths (#53)"
# exFAT, FAT and SMB have no /.vol: a file there was named by an identity
# path that did not exist, and every extended attribute call failed with
# ENOENT on a file that was plainly there.
docker pull -q "$PYTHON" >/dev/null
metadata "/Volumes/$EXFAT" "/Volumes/$APFS" | judge "exFAT"

echo
echo "==> Metadata between two SMB shares (#53)"
# The report's own setup: two shares mounted with mount_smbfs, bind-mounted
# into one container. The server is Samba in a container on this machine,
# published on 445, so the gate needs no NAS.
if lsof -nP -iTCP:445 -sTCP:LISTEN >/dev/null 2>&1; then
	fail "port 445 is taken on this Mac (File Sharing?); the SMB checks need it"
else
	docker build -q -t lighter-gate-smb scripts/gates/fixtures/smb >/dev/null
	docker run -d --name lighter-gate-smb -p 445:445 lighter-gate-smb >/dev/null
	for _ in $(seq 1 40); do nc -z 127.0.0.1 445 2>/dev/null && break; sleep 0.25; done
	mkdir -p "$SMB_D" "$SMB_T"
	# By name rather than address: the Mac's SMB client keeps a session to
	# a server it has just used, and refuses a second mount of the same
	# share by the same spelling until it lets go ("File exists").
	if mount_smbfs "//lt:lt@localhost/d" "$SMB_D" 2>"$SCRATCH/smb.err" \
		&& mount_smbfs "//lt:lt@localhost/t" "$SMB_T" 2>>"$SCRATCH/smb.err"; then
		metadata "$SMB_D" "$SMB_T" | judge "SMB"
		# A network volume that stops answering stops only what touches
		# it. Served on the vCPU that asked, a request on it stopped that
		# CPU for as long: a NAS that hung for 48 s froze three of the
		# guest's CPUs, one for 40, and the guest logged RCU stalls.
		# And it holds only its share of the workers: with every worker
		# of a share waiting on a hung NAS, a file on the Mac's own disk in
		# the same share waited 38.7 s behind them. Forty lookups in forty
		# directories, so the guest cannot fold them into a few.
		for i in $(seq 1 40); do mkdir -p "$SMB_D/d$i"; done
		LOCAL_PROBE="$HOME/.lighter-gate-local-$$"
		mkdir -p "$LOCAL_PROBE" && touch "$LOCAL_PROBE/probe"
		docker run -d --name lighter-gate-smb-reader -v "$SMB_D:/m" "$IMAGE" sh -c \
			'for i in $(seq 1 40); do (while true; do stat /m/d$i/fresh-$RANDOM$RANDOM >/dev/null 2>&1; done) & done; wait' >/dev/null
		docker run -d --name lighter-gate-local -v "$LOCAL_PROBE:/h" "$PYTHON" python3 -c '
import os, time
stop = time.time() + 35; worst = 0.0
while time.time() < stop:
    t = time.monotonic(); os.stat("/h/probe"); os.listdir("/h"); worst = max(worst, time.monotonic() - t)
    time.sleep(0.05)
print("%.2f" % worst)' >/dev/null
		docker run -d --name lighter-gate-ticker "$PYTHON" python3 -c '
import os, threading, time
n = os.cpu_count(); worst = [0.0] * n; stop = time.time() + 35
def tick(cpu):
    os.sched_setaffinity(0, {cpu}); last = time.monotonic()
    while time.time() < stop:
        time.sleep(0.05); now = time.monotonic(); worst[cpu] = max(worst[cpu], now - last); last = now
ts = [threading.Thread(target=tick, args=(c,)) for c in range(n)]
[t.start() for t in ts]; [t.join() for t in ts]
print("%.2f" % max(worst))' >/dev/null
		sleep 5
		docker pause lighter-gate-smb >/dev/null
		sleep 20
		# Measured only if the share lasted the hang: macOS sometimes gives
		# up on a server that stops answering and drops the share, and the
		# requests then fail at once, which proves nothing either way.
		kept=0
		grep -q "on $SMB_D (smbfs" <<<"$(mount)" && kept=1
		docker unpause lighter-gate-smb >/dev/null
		docker wait lighter-gate-ticker lighter-gate-local >/dev/null
		gap="$(docker logs lighter-gate-ticker 2>&1 | tail -1)"
		slowest="$(docker logs lighter-gate-local 2>&1 | tail -1)"
		docker rm -f lighter-gate-ticker lighter-gate-smb-reader lighter-gate-local >/dev/null
		rm -rf "$LOCAL_PROBE"
		if [ "$kept" = 1 ]; then
			awk -v g="$gap" 'BEGIN { exit !(g + 0 < 2 && g != "") }' \
				&& pass "SMB: the server hung for 20 s, and no guest CPU stopped (longest gap ${gap} s)" \
				|| fail "SMB: with the server hung, a guest CPU stopped for ${gap:-?} s"
			awk -v g="$slowest" 'BEGIN { exit !(g + 0 < 2 && g != "") }' \
				&& pass "SMB: and a file on the Mac's own disk in the same share kept answering (slowest ${slowest} s)" \
				|| fail "SMB: with the server hung, a local file in the same share waited ${slowest:-?} s"
		else
			note "SMB: macOS dropped the share while its server was hung, so the hang was not measured (gap ${gap:-?} s, slowest local ${slowest:-?} s)"
		fi
		umount "$SMB_D" 2>/dev/null || true
		umount "$SMB_T" 2>/dev/null || true
	else
		fail "could not mount the shares: $(cat "$SCRATCH/smb.err")"
	fi
	docker rm -f lighter-gate-smb >/dev/null
fi

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
echo "==> What a database does on a drive (#69)"
# InnoDB opens its files O_DIRECT and preallocates them at once; ClickHouse
# writes its metadata with a rename that must not replace (renameat2 is 276).
for volume in "$APFS" "$EXFAT"; do
	got="$(docker run --rm -v "/Volumes/$volume:/d" "$PYTHON" python3 -c '
import ctypes, os
fd = os.open("/d/ibdata1", os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_DIRECT, 0o660)
os.posix_fallocate(fd, 0, 12 << 20)
print("size", os.fstat(fd).st_size)
libc = ctypes.CDLL(None, use_errno=True)
def rename_noreplace(a, b):
    return libc.syscall(276, -100, a.encode(), -100, b.encode(), 1) == 0 or ctypes.get_errno()
open("/d/meta.tmp", "w").write("x")
print("noreplace", rename_noreplace("/d/meta.tmp", "/d/meta"))
open("/d/meta2.tmp", "w").write("y")
print("onto", rename_noreplace("/d/meta2.tmp", "/d/meta"))
' 2>&1 | xargs || true)"
	[ "$got" = "size 12582912 noreplace True onto 17" ] \
		&& pass "$volume: an O_DIRECT file is created and preallocated; a rename that must not replace does not" \
		|| fail "$volume: $got"
	rm -f "/Volumes/$volume/ibdata1" "/Volumes/$volume/meta" "/Volumes/$volume/meta2.tmp"
done
# exFAT keeps no permissions: a container's chmod is kept in its record. The
# shape of Borg Backup Server's start: a directory for one service user inside
# one owned by another, reached by the first.
got="$(docker run --rm -v "/Volumes/$EXFAT:/d" "$PYTHON" python3 -c '
import os
os.makedirs("/d/srv/db")
os.chown("/d/srv", 33, 33); os.chmod("/d/srv", 0o755)
os.chown("/d/srv/db", 100, 101); os.chmod("/d/srv/db", 0o700)
pid = os.fork()
if pid == 0:
    os.setgid(101); os.setuid(100)
    try:
        open("/d/srv/db/aria_log_control", "w").write("x")
        os._exit(0)
    except OSError as e:
        print(e); os._exit(1)
_, status = os.waitpid(pid, 0)
print(oct(os.stat("/d/srv").st_mode & 0o777), oct(os.stat("/d/srv/db").st_mode & 0o777), os.waitstatus_to_exitcode(status))
' 2>&1 | xargs || true)"
[ "$got" = "0o755 0o700 0" ] && pass "exFAT: a chmod is kept, and a service user reaches its own directory" || fail "exFAT permissions: $got"
again="$(docker run --rm -v "/Volumes/$EXFAT:/d" "$IMAGE" stat -c '%a %u' /d/srv /d/srv/db 2>&1 | xargs || true)"
[ "$again" = "755 33 700 100" ] && pass "exFAT: and another container sees it" || fail "exFAT, another container: $again"
rm -rf "/Volumes/$EXFAT/srv"

echo
echo "==> A container's nested mounts outlive a change on the Mac (#70)"
NEST="$SCRATCH/nested"
mkdir -p "$NEST/sess/x/repo" "$NEST/repo"
echo hello > "$NEST/repo/hello.txt"
docker run -d --name lighter-gate-nested -v "$NEST/sess:/w" -v "$NEST/repo:/w/x/repo:ro" "$IMAGE" sleep 600 >/dev/null
docker exec lighter-gate-nested cat /w/x/repo/hello.txt >/dev/null
touch "$NEST/sess/x/repo" "$NEST/sess/x"
xattr -w sh.lighter.gate 1 "$NEST/sess/x"
sleep 2
got="$(docker exec lighter-gate-nested sh -c 'grep -c " /w/x/repo " /proc/self/mountinfo; cat /w/x/repo/hello.txt' 2>&1 | xargs || true)"
[ "$got" = "1 hello" ] && pass "the Mac touched and tagged the directories a mount sits in, and it is still there" \
	|| fail "after the Mac touched the directories a mount sits in: $got"
echo gone > "$NEST/sess/x/after.txt"
rm -rf "$NEST/sess/x/after.txt"
mkdir "$NEST/sess/fresh"
sleep 1.5
got="$(docker exec lighter-gate-nested sh -c 'test -e /w/x/after.txt && echo stale || echo gone; test -d /w/fresh && echo seen' 2>&1 | xargs || true)"
[ "$got" = "gone seen" ] && pass "and names the Mac removed and made are seen as such" || fail "names after the touch: $got"
docker rm -f lighter-gate-nested >/dev/null

echo
echo "==> Owners and modes outlive a restart"
# The shares' roots (/Users, /Volumes) are root's, so nothing marks them as
# holding records; until 0.12.4 every recorded owner read as root after a
# restart, until the next chown.
mkdir -p "$SCRATCH/owned/pg" "/Volumes/$EXFAT/kept"
docker run --rm -v "$SCRATCH/owned:/h" -v "/Volumes/$EXFAT:/d" "$IMAGE" sh -c \
	'chown 999:999 /h/pg && chown 100:101 /d/kept && chmod 751 /d/kept' >/dev/null
"$LIGHTER" restart >/dev/null 2>&1
got="$(docker run --rm -v "$SCRATCH/owned:/h" -v "/Volumes/$EXFAT:/d" "$IMAGE" stat -c '%u:%g %a' /h/pg /d/kept 2>&1 | xargs || true)"
[ "$got" = "999:999 755 100:101 751" ] && pass "after a restart: a home-folder owner, and an exFAT owner and mode" \
	|| fail "after a restart: $got (want 999:999 755 100:101 751)"
rm -rf "/Volumes/$EXFAT/kept"

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
if grep -qx "$EXFAT" <<<"$(docker run --rm -v /Volumes:/v "$IMAGE" ls /v 2>/dev/null)"; then
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
if grep -q "lighter-gate-reader\|/Volumes/$EXFAT" <<<"$("$LIGHTER" status)"; then
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
doctor="$("$LIGHTER" doctor 2>&1 || true)"
if grep -q "bind mounts.*every one from the Mac is shared" <<<"$doctor"; then
	pass "and stops once the containers have gone"
else
	fail "lighter doctor still warns with the containers gone"
	sed 's/^/    /' <<<"$doctor"
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
doctor="$("$LIGHTER" doctor 2>&1 || true)"
if grep -q "not there, so not shared: $MISSING" <<<"$doctor"; then
	pass "lighter doctor names it"
else
	fail "lighter doctor does not name the missing share"
	sed 's/^/    /' <<<"$doctor"
fi
"$LIGHTER" config --unshare "$MISSING" >/dev/null \
	&& pass "lighter config --unshare removes it" \
	|| fail "lighter config --unshare could not remove it"

if [ "$FAILED" = 0 ]; then
	echo
	echo "m16: shared folders and drives work"
fi
exit "$FAILED"
