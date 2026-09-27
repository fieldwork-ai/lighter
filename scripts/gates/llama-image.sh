#!/usr/bin/env bash
# Makes sure the llama.cpp image gate m12 and benchmarks/llm-gpu.sh run is on
# an engine, from the recipe in this repository rather than from a tarball
# somebody once built by hand (which is how it was, and no fresh checkout
# could pass m12).
#
#   scripts/gates/llama-image.sh [docker arguments...]    e.g. --context lighter
#
# An image already on the engine is used if its label names this recipe and
# llama.cpp commit; otherwise a tarball kept under ~/.cache/lighter is loaded;
# otherwise the image is built on the engine, once, and saved there for next
# time. LIGHTER_GATE_LLAMA_IMAGE_TAR loads a given tarball instead.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE="${LIGHTER_GATE_LLAMA_IMAGE:-llama-vulkan:arm64}"
RECIPE="$ROOT/scripts/gates/fixtures/llama-vulkan.Dockerfile"
COMMIT="$(sed -n 's/^LLAMA_COMMIT="${LLAMA_COMMIT:-\([0-9a-f]*\)}"/\1/p' "$ROOT/host/metal/build.sh")"
[ -n "$COMMIT" ] || { echo "cannot read the llama.cpp commit from host/metal/build.sh" >&2; exit 1; }
KEY="$( { cat "$RECIPE"; echo "$COMMIT"; } | shasum -a 256 | cut -c1-12)"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/lighter/llama-vulkan-$KEY.tar"
dk() { docker "$@"; }

if [ -n "${LIGHTER_GATE_LLAMA_IMAGE_TAR:-}" ]; then
	dk "$@" image inspect "$IMAGE" >/dev/null 2>&1 || dk "$@" load -q -i "$LIGHTER_GATE_LLAMA_IMAGE_TAR" >/dev/null
	exit 0
fi
[ "$(dk "$@" image inspect -f '{{index .Config.Labels "lighter.llama"}}' "$IMAGE" 2>/dev/null)" != "$KEY" ] || exit 0
if [ -f "$CACHE" ]; then
	dk "$@" load -q -i "$CACHE" >/dev/null
	exit 0
fi
echo "building $IMAGE (llama.cpp $COMMIT) from $RECIPE; once, kept as $CACHE" >&2
context="$(mktemp -d)"
trap 'rm -rf "$context"' EXIT
dk "$@" build -q -t "$IMAGE" --build-arg "LLAMA_COMMIT=$COMMIT" --label "lighter.llama=$KEY" \
	-f - "$context" < "$RECIPE" >/dev/null
mkdir -p "$(dirname "$CACHE")"
dk "$@" save -o "$CACHE.part" "$IMAGE" && mv "$CACHE.part" "$CACHE"
