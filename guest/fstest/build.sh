#!/usr/bin/env bash
# Build the guest coherence prober, a static linux-aarch64 binary.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/guest/out"
HERE="$ROOT/guest/fstest"

mkdir -p "$OUT"

echo "==> Building guest fs prober"
docker build --quiet --output "type=local,dest=$OUT" "$HERE" >/dev/null

chmod +x "$OUT/lighter-fstest"
echo "==> $(du -h "$OUT/lighter-fstest" | cut -f1) prober at $OUT/lighter-fstest"
