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

# One kernel ships: `Image`, at 250 Hz. A 1000 Hz twin was built and shipped
# beside it for a day (faster container starts, slower share installs;
# `docs/architecture.md`, "One kernel") and dropped; `LIGHTER_KERNEL_ONLY=1000`
# still builds it, on its own source volume, for an A/B.
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
		-e "KERNEL_TRACE=${KERNEL_TRACE:-}" \
		"$IMAGE"
}
case "${LIGHTER_KERNEL_ONLY:-both}" in
	250)  build 250 "" "$VOLUME" ;;
	1000) build 1000 "-hz1000" "$VOLUME-hz1000" ;;
	# `Image-trace`: the 250 Hz kernel with ftrace and its tracepoints, for
	# finding what wakes an idle guest (timer expiries, work items by
	# function) — never shipped, never a record's kernel.
	trace) KERNEL_TRACE=1 build 250 "-trace" "$VOLUME-trace" ;;
	*)    build 250 "" "$VOLUME" ;;
esac

echo
echo "==> Artifacts in $OUT:"
ls -la "$OUT"
