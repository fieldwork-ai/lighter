#!/usr/bin/env bash
# Build the guest root filesystem image.
#
# Produces guest/out/rootfs.ext4: Alpine, dockerd, and our agent, as an ext4
# filesystem a guest can mount read-write on /dev/vda.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/guest/out"
HERE="$ROOT/guest/rootfs"

mkdir -p "$OUT"

# The agent is built by its own toolchain; this image only packages it.
[ -f "$OUT/lighter-agent" ] || "$ROOT/guest/agent/build.sh"
cp "$OUT/lighter-agent" "$HERE/lighter-agent"
trap 'rm -f "$HERE/lighter-agent"' EXIT

echo "==> Building root filesystem"
docker build --quiet --output "type=local,dest=$OUT" "$HERE" >/dev/null

echo "==> $(du -h "$OUT/rootfs.ext4" | cut -f1) rootfs at $OUT/rootfs.ext4"
