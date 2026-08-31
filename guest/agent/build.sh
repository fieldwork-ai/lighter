#!/usr/bin/env bash
# Build the guest agent, a static linux-aarch64 binary.
#
# In a container because it is a Linux binary and the host is macOS, and because
# pinning the toolchain in an image is what makes the artifact reproducible.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/guest/out"
HERE="$ROOT/guest/agent"

mkdir -p "$OUT"

echo "==> Building guest agent"
docker build --quiet --output "type=local,dest=$OUT" "$HERE" >/dev/null

chmod +x "$OUT/lighter-agent"
echo "==> $(du -h "$OUT/lighter-agent" | cut -f1) agent at $OUT/lighter-agent"
