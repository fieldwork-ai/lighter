#!/usr/bin/env bash
# Build lighter's own guest, using lighter.
#
#   scripts/dogfood.sh              # make guest
#   scripts/dogfood.sh make guest   # the same, said out loud
#   scripts/dogfood.sh bash         # a shell with DOCKER_HOST pointed at us
#
# Until now the kernel, the initramfs and the root filesystem were all built
# inside containers on somebody else's virtual machine — Colima, in practice.
# That was the bootstrap, and it has outstayed its welcome: a container runtime
# that can run `docker compose` can build a kernel, and the fastest way to find
# out whether ours is any good at real work is to give it the heaviest job in
# the repository and ask it to reproduce itself.
#
# # The two things that make it work
#
# **The repository is shared at its own absolute path.** `guest/kernel/build.sh`
# bind-mounts `$ROOT/guest/out` into its builder, and that path is a macOS path.
# Mounting the share at the identical path inside the guest means every script
# here works unchanged, with no notion that it is running anywhere unusual.
#
# **The VM boots from a copy of the root filesystem.** The build writes
# `guest/out/rootfs.ext4`, and that is the disk this VM would be running from.
# Overwriting the block device underneath a live guest corrupts it in a way that
# surfaces hours later as a filesystem that will not mount. The copy is a
# clone on APFS, so it costs nothing.
set -euo pipefail

if ! command -v cargo >/dev/null 2>&1; then
	# shellcheck disable=SC1091
	. "$HOME/.cargo/env"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

KERNEL="guest/out/Image"
ROOTFS="guest/out/rootfs.ext4"
GVPROXY="${GVPROXY:-vendor/gvproxy}"
BIN="target/release/examples/lighter-bench"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-180}"
CPUS="${CPUS:-$(sysctl -n hw.ncpu)}"
MEMORY_MIB="${MEMORY_MIB:-12288}"

for artifact in "$KERNEL" "$ROOTFS"; do
	[ -f "$artifact" ] || {
		echo "$artifact is missing; there is nothing to bootstrap from." >&2
		echo "Build it once the old way first: make guest" >&2
		exit 1
	}
done
[ -x "$GVPROXY" ] || { echo "gvproxy missing; run scripts/fetch-gvproxy.sh" >&2; exit 1; }

echo "==> Building and signing the VMM"
cargo build --release --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

RUN_DIR="$(mktemp -d -t lighter-dogfood)"
SOCKET="$RUN_DIR/docker.sock"
BOOT_LOG="$RUN_DIR/boot.log"
VMM_PID=""

cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	rm -rf "$RUN_DIR"
}
trap cleanup EXIT

# `cp -c` asks APFS to clone rather than copy, so a gigabyte costs nothing and
# no blocks are duplicated until one side writes.
echo "==> Cloning the root filesystem so the build can overwrite the original"
cp -c "$ROOTFS" "$RUN_DIR/root.img" 2>/dev/null || cp "$ROOTFS" "$RUN_DIR/root.img"

echo "==> Booting lighter (${CPUS} cores, ${MEMORY_MIB} MiB)"
: > "$BOOT_LOG"
"$BIN" \
	--kernel "$KERNEL" \
	--disk "$RUN_DIR/root.img" \
	--disk "$RUN_DIR/data.img" --disk-size-gib 96 \
	--net "$GVPROXY" --run-dir "$RUN_DIR" \
	--vsock "$SOCKET:2375" \
	--share repo:"$ROOT" \
	--no-tty --cpus "$CPUS" --memory-mib "$MEMORY_MIB" \
	--cmdline "console=ttyAMA0 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s) lighter.share=repo:$ROOT" \
	>"$BOOT_LOG" 2>&1 &
VMM_PID=$!
disown "$VMM_PID" 2>/dev/null || true

waited=0
while ! grep -q "AGENT listening" "$BOOT_LOG" 2>/dev/null; do
	kill -0 "$VMM_PID" 2>/dev/null || { echo "the VMM exited during boot" >&2; tail -20 "$BOOT_LOG" >&2; exit 1; }
	[ "$waited" -lt "$BOOT_TIMEOUT" ] || { echo "lighter did not come up" >&2; tail -20 "$BOOT_LOG" >&2; exit 1; }
	sleep 1
	waited=$((waited + 1))
done

if ! grep -q "INIT share=repo at=$ROOT\b" "$BOOT_LOG"; then
	echo "the repository is not mounted in the guest; the build would write nowhere" >&2
	grep -a "INIT share" "$BOOT_LOG" >&2 || true
	exit 1
fi

export DOCKER_HOST="unix://$SOCKET"
echo "==> lighter is up in ${waited}s, and is now the Docker daemon"
docker version --format '    {{.Server.Version}} on {{.Server.Os}}/{{.Server.Arch}}'

started="$(date +%s)"
if [ "$#" -eq 0 ]; then
	make guest
else
	"$@"
fi
echo
echo "==> lighter built its own guest in $(( $(date +%s) - started ))s"
