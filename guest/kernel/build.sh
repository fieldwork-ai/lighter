#!/usr/bin/env bash
# Build the lighter guest kernel via a container.
#
# The source tree lives in a named Docker volume, not a bind mount. Two
# reasons, and the second is the interesting one:
#
#   1. Extracting a kernel tarball onto a macOS bind mount fails outright —
#      virtiofs cannot reproduce the ownership and symlinks the archive carries.
#   2. A kernel tree is ~80,000 files and a build stats all of them repeatedly.
#      On a bind mount that is minutes of pure boundary crossing.
#
# Which is the problem lighter exists to fix. Until it fixes it, we route
# around it the same way every Docker user does: keep hot data on the guest's
# own filesystem and share only the result.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/guest/out"
VOLUME="${LIGHTER_KERNEL_VOLUME:-lighter-kernel-src}"
IMAGE="lighter-kernel-builder"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"

mkdir -p "$OUT"

echo "==> Building builder image"
docker build -q -t "$IMAGE" "$ROOT/guest/kernel" >/dev/null

# Two kernels from one source, differing in the tick rate alone: `Image` at
# 250 Hz for a Mac whose vCPUs fill its cores, `Image-hz1000` for one where
# they leave half of them free (the CLI's `config::kernel_hz`). Each has its
# own source volume, since a change of HZ rebuilds most of the tree and one
# volume would rebuild it every time. `LIGHTER_KERNEL_ONLY=250|1000` builds
# just the one, for iteration.
build() {
	local hz="$1" suffix="$2" volume="$3"
	echo "==> Building kernel at ${hz} Hz (source volume: $volume, jobs: $JOBS)"
	docker run --rm \
		--name "lighter-kbuild-$hz" \
		-v "$volume:/build" \
		-v "$OUT:/out" \
		-e "JOBS=$JOBS" \
		-e "KERNEL_VERSION=${KERNEL_VERSION:-6.18.49}" \
		-e "KERNEL_HZ=$hz" \
		-e "KERNEL_IMAGE_SUFFIX=$suffix" \
		"$IMAGE"
}
case "${LIGHTER_KERNEL_ONLY:-both}" in
	250)  build 250 "" "$VOLUME" ;;
	1000) build 1000 "-hz1000" "$VOLUME-hz1000" ;;
	*)    build 250 "" "$VOLUME"; build 1000 "-hz1000" "$VOLUME-hz1000" ;;
esac

echo
echo "==> Artifacts in $OUT:"
ls -la "$OUT"
