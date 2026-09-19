#!/usr/bin/env bash
# Build the guest's ONNX Runtime plugin execution provider: a no_std Rust
# shared object linking no libc, so the one file loads in any container.
# Output: guest/out/liblighter_ane_ep.so, which the root filesystem carries
# at /usr/lib/lighter/.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/guest/out"
mkdir -p "$OUT"
echo "==> Building the neural engine plugin EP"
docker run --rm \
	-v "$ROOT/guest/ane-ep:/src:ro" -v "$OUT:/src/../out" \
	-v "lighter-ane-ep-target:/target" \
	-v "lighter-cargo-registry:/usr/local/cargo/registry" \
	-e RUSTFLAGS="-C link-arg=-nostdlib -C link-arg=-nostartfiles" \
	-e CARGO_TARGET_DIR=/target \
	-w /src \
	rust:1-slim-bookworm \
	sh -c 'cargo build --release --quiet && cp /target/release/liblighter_ane_ep.so /src/../out/liblighter_ane_ep.so' 2>&1 | tail -5
# The library must depend on nothing: the check that keeps it loadable everywhere.
if docker run --rm -v "$OUT:/out:ro" rust:1-slim-bookworm sh -c 'readelf -d /out/liblighter_ane_ep.so | grep -q NEEDED'; then
	echo "error: the plugin EP links a shared library; it must not" >&2
	exit 1
fi
ls -la "$OUT/liblighter_ane_ep.so"
