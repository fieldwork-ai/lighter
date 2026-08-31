#!/usr/bin/env bash
# Build the boot initramfs: a static busybox and an init script.
#
# The cpio archive is assembled inside a container rather than on macOS,
# because the archive records ownership and device nodes that HFS/APFS and a
# non-root user cannot express. Building it on the host produces an image whose
# /dev is wrong in ways that only show up as a guest with no console.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/guest/out"
HERE="$ROOT/guest/initramfs"

mkdir -p "$OUT"

# The agent is a separate build with its own toolchain; the initramfs just
# packages it. Building it here rather than requiring the caller to remember is
# what keeps `make gate-m3-vsock` a single command.
[ -f "$OUT/lighter-agent" ] || "$ROOT/guest/agent/build.sh"
[ -f "$OUT/lighter-fstest" ] || "$ROOT/guest/fstest/build.sh"
cp "$OUT/lighter-agent" "$HERE/lighter-agent"
cp "$OUT/lighter-fstest" "$HERE/lighter-fstest"
trap 'rm -f "$HERE/lighter-agent" "$HERE/lighter-fstest"' EXIT

echo "==> Building initramfs"
# `--output type=local` exports the final scratch stage's files directly, which
# avoids needing a shell in an image that deliberately has nothing in it.
docker build --quiet --output "type=local,dest=$OUT" "$HERE" >/dev/null

echo "==> $(du -h "$OUT/initramfs.cpio.gz" | cut -f1) initramfs at $OUT/initramfs.cpio.gz"
