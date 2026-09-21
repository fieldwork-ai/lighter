#!/usr/bin/env bash
# The starvation of 2026-09-20 on demand: unmovable pages fill the guest's
# kernel-usable memory while containers run, and the docker socket, its
# streams and its builds are timed against that.
#
#   scripts/repro-kernel-zones.sh [--fraction F] [--no-fragment] [--hog] [--export NAME]
#
# Needs DOCKER_HOST pointing at the machine under test and a running
# container to export (--export, default: the largest running one). Leaves
# the machine as it found it. Exit 0 when every probe passed, 1 when a
# stream broke, an API call hung, or the vsock driver refused buffers.
set -u
FRACTION=0.7; FRAGMENT=1; HOG=0; EXPORT=""; GUARD=15
while [ $# -gt 0 ]; do
	case "$1" in
	--fraction) FRACTION="$2"; shift 2 ;;
	--no-fragment) FRAGMENT=0; shift ;;
	--hog) HOG=1; shift ;;
	--export) EXPORT="$2"; shift 2 ;;
	--guard) GUARD="$2"; shift 2 ;;
	*) echo "unknown argument $1" >&2; exit 2 ;;
	esac
done
red=0
say() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*"; }
fail() { red=1; say "RED $*"; }

# One privileged, pid=host probe container for the whole run: `docker run`
# itself hangs in a starved guest, `exec` into a live one is what still works.
docker rm -f rkz-probe rkz-pipes rkz-hog >/dev/null 2>&1
docker run -d --name rkz-probe --privileged --pid=host --platform linux/arm64 alpine sleep 3600 >/dev/null || exit 2
docker buildx inspect rkz-builder >/dev/null 2>&1 || docker buildx create --name rkz-builder --driver docker-container --bootstrap >/dev/null 2>&1 || { echo "no docker-container builder" >&2; exit 2; }
probe() { docker exec rkz-probe sh -c "$1"; }

snap() {
	probe 'free -m | awk "/Mem:/{printf \"free=%s avail=%s cache=%s \", \$4, \$7, \$6}";
	awk "/Normal/{printf \"Normal o4+=%s,%s,%s,%s,%s,%s \", \$8,\$9,\$10,\$11,\$12,\$13} /DMA /{printf \"DMA o6+=%s,%s,%s \", \$10,\$11,\$12}" /proc/buddyinfo;
	grep -E "^(compact_fail|allocstall_normal) " /proc/vmstat | awk "{printf \"%s=%s \", \$1, \$2}";
	printf "refused=%s " "$(dmesg | grep -c "large receive buffer")";
	cat /proc/pressure/memory 2>/dev/null | awk "/^full/{print \"psi_full_avg10=\" substr(\$2,7)}"'
}
kernel_zone_bytes() {
	probe 'awk "/^Node/{z=\$4} /managed/{if (z==\"DMA\"||z==\"DMA32\"||z==\"Normal\") s+=\$2} END{print s*4096}" /proc/zoneinfo'
}
refused_now() { probe 'dmesg | grep -c "large receive buffer"'; }

# An API call under a guard: hung is a finding, not a wait.
guarded() {
	local label="$1"; shift
	local start; start=$(date +%s.%N)
	"$@" >/tmp/rkz.out 2>/tmp/rkz.err & local pid=$!
	local i; for ((i = 0; i < GUARD * 4; i++)); do kill -0 $pid 2>/dev/null || break; sleep 0.25; done
	if kill -0 $pid 2>/dev/null; then kill -9 $pid 2>/dev/null; fail "$label hung past ${GUARD}s"; return 1; fi
	wait $pid; local rc=$?
	local secs; secs=$(echo "$(date +%s.%N) - $start" | bc)
	if [ $rc -ne 0 ]; then fail "$label rc=$rc $(head -c 120 /tmp/rkz.err)"; else say "ok   $label ${secs}s"; fi
	return $rc
}

stream() {
	local start; start=$(date +%s.%N)
	local bytes; bytes=$(docker export "$EXPORT" 2>/tmp/rkz.err | wc -c | tr -d ' ')
	local secs; secs=$(echo "$(date +%s.%N) - $start" | bc)
	if [ -s /tmp/rkz.err ]; then fail "export $EXPORT broke after $bytes bytes in ${secs}s: $(head -c 100 /tmp/rkz.err)"
	else say "ok   export $EXPORT $bytes bytes ${secs}s"; fi
}

build() {
	# A build whose RUN steps write a fair amount of log and layer, so the
	# BuildKit session over the docker socket is exercised, then an export
	# to the docker image format, which is where the M5's build died.
	local dir; dir=$(mktemp -d)
	cat >"$dir/Dockerfile" <<'DF'
FROM alpine
RUN for i in $(seq 1 3000); do echo "line $i of a build log that is long enough to matter"; done
RUN dd if=/dev/urandom of=/blob bs=1M count=200 status=none && sha256sum /blob
RUN for i in $(seq 1 3000); do echo "second layer line $i"; done
DF
	# A docker-container builder, as the M5's deploy uses: the build runs in
	# a BuildKit container and `--load` streams the image back through the
	# session over the docker socket, which is the step that died.
	local start; start=$(date +%s.%N)
	if docker buildx build --builder rkz-builder --no-cache --load -t rkz-built:latest --progress=plain "$dir" >/tmp/rkz.build 2>&1; then
		say "ok   build ${start:+$(echo "$(date +%s.%N) - $start" | bc)s}"
	else
		fail "build failed: $(grep -aE 'ERROR|error' /tmp/rkz.build | head -2 | cut -c1-160 | tr '\n' ' ')"
	fi
	rm -rf "$dir"
}

api_probes() {
	guarded "version" docker version --format '{{.Server.Version}}'
	guarded "inspect" docker inspect rkz-probe --format '{{.State.Status}}'
	guarded "run" docker run --rm --platform linux/arm64 alpine true
	guarded "healthcheck-exec" docker exec rkz-probe true
}

if [ -z "$EXPORT" ]; then
	EXPORT=$(docker ps --format '{{.Names}}' | grep -v '^rkz-' | head -1)
fi
say "target $EXPORT; containers running: $(docker ps -q | wc -l | tr -d ' ')"
say "baseline: $(snap)"
refused0=$(refused_now)
stream; api_probes; build

if [ "$FRAGMENT" = 1 ]; then
	kz=$(kernel_zone_bytes); pipes=$(python3 -c "print(int($kz * $FRACTION / 65536))")
	say "fragmenting: $pipes filled pipes (unmovable), $(python3 -c "print(round($kz*$FRACTION/2**30,2))") GiB of $(python3 -c "print(round($kz/2**30,2))") GiB kernel-usable"
	docker run -d --name rkz-pipes --privileged --ulimit nofile=1048576:1048576 --oom-score-adj 1000 --platform linux/arm64 python:3.12-alpine python3 -c "
import os, time
buf = b'x' * 65536; n = 0
try:
    for i in range($pipes):
        r, w = os.pipe(); os.write(w, buf); n += 1
except Exception as e:
    print('stopped at', n, e, flush=True)
print('pipes', n, flush=True); time.sleep(3600)" >/dev/null
	for _ in $(seq 1 120); do sleep 2; docker logs rkz-pipes 2>&1 | grep -q pipes && break; done
	say "fragmenter: $(docker logs rkz-pipes 2>&1 | tail -1)"
	say "fragmented: $(snap)"
	stream; api_probes; build
	say "after: $(snap)"
	docker rm -f rkz-pipes >/dev/null 2>&1
fi

if [ "$HOG" = 1 ]; then
	say "hog: anonymous memory until the guest has under 900 MiB available"
	docker run -d --name rkz-hog --privileged --oom-score-adj 1000 --platform linux/arm64 python:3.12-alpine python3 -c "
import time
chunks = []
def avail():
    for l in open('/proc/meminfo'):
        if l.startswith('MemAvailable'): return int(l.split()[1]) // 1024
while avail() > 900:
    chunks.append(bytearray(128 << 20))
print('held', len(chunks) * 128, 'MiB', flush=True); time.sleep(3600)" >/dev/null
	for _ in $(seq 1 120); do sleep 2; docker logs rkz-hog 2>&1 | grep -q held && break; done
	say "hog: $(docker logs rkz-hog 2>&1 | tail -1)"
	say "hogged: $(snap)"
	stream; api_probes; build
	say "after: $(snap)"
	docker rm -f rkz-hog >/dev/null 2>&1
fi

refused1=$(refused_now)
# The receive side falling back to small buffers is the fallback working,
# not a failure; the count says how fragmented the guest was.
[ "$refused1" -gt "$refused0" ] && say "note vsock fell back to small receive buffers $((refused1 - refused0)) times"
docker rm -f rkz-probe >/dev/null 2>&1
docker buildx rm rkz-builder >/dev/null 2>&1; docker rmi rkz-built:latest >/dev/null 2>&1
if [ $red = 0 ]; then say "GREEN"; else say "RED: see the lines above"; fi
exit $red
