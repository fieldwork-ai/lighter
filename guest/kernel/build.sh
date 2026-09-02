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

echo "==> Building kernel (source volume: $VOLUME, jobs: $JOBS)"
docker run --rm \
	--name lighter-kbuild \
	-v "$VOLUME:/build" \
	-v "$OUT:/out" \
	-e "JOBS=$JOBS" \
	-e "KERNEL_VERSION=${KERNEL_VERSION:-6.18.49}" \
	"$IMAGE"

echo
echo "==> Artifacts in $OUT:"
ls -la "$OUT"
