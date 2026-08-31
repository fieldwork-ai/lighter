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

echo "==> Building initramfs"
# `--output type=local` exports the final scratch stage's files directly, which
# avoids needing a shell in an image that deliberately has nothing in it.
docker build --quiet --output "type=local,dest=$OUT" "$HERE" >/dev/null

echo "==> $(du -h "$OUT/initramfs.cpio.gz" | cut -f1) initramfs at $OUT/initramfs.cpio.gz"
