#!/usr/bin/env bash
# The balloon's units, and that they move: a guest fragmented by unmovable
# pages is ballooned at Warn, so the driver falls from whole pageblocks to
# smaller units; the fragmenter leaves; compaction is forced; the small
# units migrate (`balloon_migrate` in the guest's vmstat) and nothing
# warns. Boots its own machine, as the m6 gate does.
set -euo pipefail
if ! command -v cargo >/dev/null 2>&1; then . "$HOME/.cargo/env"; fi
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"; cd "$ROOT"
KERNEL="${LIGHTER_GATE_KERNEL:-guest/out/Image}"
ROOTFS="$(mktemp -t lighter-rootfs).ext4"
cp -c guest/out/rootfs.ext4 "$ROOTFS" 2>/dev/null || cp guest/out/rootfs.ext4 "$ROOTFS"
PROFILE="${PROFILE:-release}"; BIN="target/$PROFILE/examples/lighter-bench"
pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
note() { printf '  \033[33m··\033[0m   %s\n' "$*"; }
FAILED=0
cargo build $([ "$PROFILE" = release ] && echo --release) -q --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null
RUN_DIR="$(mktemp -d -t lighter-m6b)"; SOCKET="$RUN_DIR/docker.sock"; LOG="$RUN_DIR/boot.log"; VMM_PID=""
cleanup() { [ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true; mkdir -p .logs && cp "$LOG" .logs/m6b-last-boot.log 2>/dev/null || true; rm -rf "$RUN_DIR" "$ROOTFS"; }
trap cleanup EXIT; trap 'exit 143' INT TERM
field() { sed 's/\x1b\[[0-9;]*m//g' "$LOG" | grep -a "FOOTPRINT" | tail -1 | sed -n "s/.* $1=\([0-9][0-9]*\).*/\1/p"; }
PRESSURE_FILE="$RUN_DIR/pressure"; echo normal > "$PRESSURE_FILE"
echo "==> Booting with 4 GiB of guest RAM"
LIGHTER_PRESSURE_TEST_FILE="$PRESSURE_FILE" "$BIN" --kernel "$KERNEL" --disk "$ROOTFS" --disk "$RUN_DIR/data.img" --disk-size-gib 32 \
	--net --run-dir "$RUN_DIR" --vsock "$SOCKET:2375" --report-memory --no-tty --cpus 4 --memory-mib 4096 \
	--cmdline "console=ttyAMA0 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s)" >"$LOG" 2>&1 &
VMM_PID=$!; disown "$VMM_PID" 2>/dev/null || true
waited=0
while ! grep -q "AGENT listening" "$LOG" 2>/dev/null; do
	kill -0 "$VMM_PID" 2>/dev/null || { fail "the VMM exited during boot"; tail -20 "$LOG"; exit 1; }
	[ "$waited" -lt 180 ] || { fail "the guest did not come up"; exit 1; }
	sleep 1; waited=$((waited + 1))
done
export DOCKER_HOST="unix://$SOCKET"
pass "booted in ${waited}s"
docker pull --quiet python:3.12-alpine >/dev/null 2>&1 || true
docker run -d --name m6b-probe --privileged --pid=host alpine sleep 3600 >/dev/null
probe() { docker exec m6b-probe sh -c "$1"; }
vmstat() { probe "grep -E '^$1 ' /proc/vmstat | awk '{print \$2}'"; }
zones() { probe "awk '/^Node/{z=\$4} /managed/{printf \"%s=%d MiB \", z, \$2*4/1024}' /proc/zoneinfo"; }
note "zones: $(zones)"
[ "$(probe 'cat /sys/module/page_reporting/parameters/page_reporting_order')" = 4 ] && pass "free page reporting at order 4 (64 KiB)" || fail "reporting order is $(probe 'cat /sys/module/page_reporting/parameters/page_reporting_order')"
if probe "awk '/^Node/{z=\$4} /managed/{print z}' /proc/zoneinfo" | grep -q Movable; then fail "a Movable zone exists; the guest should be one zone"; else pass "one zone: no ZONE_MOVABLE"; fi
sleep 5
# A fragmented guest: unmovable pages spread through the pageblocks, so
# the balloon cannot have whole ones.
docker run -d --name m6b-pipes --privileged --ulimit nofile=1048576:1048576 python:3.12-alpine python3 -c '
import os, time
buf = b"x" * 65536; n = 0
try:
    for i in range(12000):
        r, w = os.pipe(); os.write(w, buf); n += 1
except Exception as e:
    print("stopped at", n, e, flush=True)
print("pipes", n, flush=True); time.sleep(3600)' >/dev/null
for _ in $(seq 1 60); do sleep 2; docker logs m6b-pipes 2>&1 | grep -q pipes && break; done
note "fragmenter: $(docker logs m6b-pipes 2>&1 | tail -1); buddyinfo Normal: $(probe 'grep Normal /proc/buddyinfo | cut -c1-90')"
inflate0="$(vmstat balloon_inflate)"
echo warn > "$PRESSURE_FILE"
waited=0; while [ "$(( $(vmstat balloon_inflate) - inflate0 ))" -lt $((768 * 256)) ] && [ "$waited" -lt 60 ]; do sleep 2; waited=$((waited + 2)); done
inflated=$(( ( $(vmstat balloon_inflate) - inflate0 ) / 256 ))
[ "$inflated" -ge 768 ] && pass "the balloon inflated ${inflated} MiB in ${waited}s under Warn on a fragmented guest" || fail "only ${inflated} MiB inflated in ${waited}s"
puffs="$(grep -ac 'Out of puff' "$LOG" || true)"
note "'Out of puff' lines: ${puffs}"
docker rm -f m6b-pipes >/dev/null 2>&1
sleep 3
migrate0="$(vmstat balloon_migrate)"
for _ in 1 2 3; do probe 'echo 1 > /proc/sys/vm/compact_memory' || true; sleep 2; done
migrated=$(( $(vmstat balloon_migrate) - migrate0 ))
if [ "$migrated" -gt 0 ]; then pass "compaction migrated ${migrated} balloon units (balloon_migrate)"; else fail "no balloon unit migrated under forced compaction (balloon_migrate ${migrate0} -> $(vmstat balloon_migrate))"; fi
if probe 'dmesg | grep -aiE "WARNING|BUG|Oops|refcount|bad page" | head -3' | grep -q .; then fail "the guest's kernel warned: $(probe 'dmesg | grep -aiE "WARNING|BUG|Oops|refcount|bad page" | head -1')"; else pass "no kernel warning through the migration"; fi
echo normal > "$PRESSURE_FILE"
sleep 12
[ "$(field ballooned_mib)" -ge 512 ] && pass "Normal is a plateau: balloon holds $(field ballooned_mib) MiB 12s later" || fail "the balloon fell to $(field ballooned_mib) MiB on Normal"
# The balloon comes down on the guest's release, then the freed units are reported.
docker run -d --name m6b-work --tmpfs /w:rw,size=1600m alpine sh -c 'dd if=/dev/zero of=/w/x bs=1M count=1400 2>/dev/null; sleep 60' >/dev/null 2>&1 || true
sleep 25
note "after a 1.4 GiB tmpfs: balloon $(field ballooned_mib) MiB, footprint $(field mib) MiB, migrated total $(vmstat balloon_migrate), deflated $(( $(vmstat balloon_deflate) / 256 )) MiB"
docker rm -f m6b-work m6b-probe >/dev/null 2>&1
echo
if [ "$FAILED" -eq 0 ]; then printf '\033[32mm6b balloon units gate passed\033[0m\n'; else printf '\033[31mm6b balloon units gate failed\033[0m\n'; exit 1; fi
