#!/usr/bin/env bash
# Build ggml with its Metal and RPC backends as static archives lighter links
# for `lighter.sh/metal`: host/out/ggml/lib*.a, from llama.cpp at a pinned
# commit. The RPC protocol is versioned, so the pin is what a container's
# llama.cpp has to match; docs/gpu.md names it.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUILD="$ROOT/host/build"
OUT="$ROOT/host/out/ggml"
LLAMA_COMMIT="${LLAMA_COMMIT:-1af554f}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-15.0}"
mkdir -p "$BUILD" "$OUT"
log() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

log "Python tools (cmake, ninja)"
if [ ! -x "$BUILD/venv/bin/cmake" ]; then
	python3 -m venv "$BUILD/venv"
	"$BUILD/venv/bin/pip" install -q --upgrade pip
	"$BUILD/venv/bin/pip" install -q cmake ninja
fi
export PATH="$BUILD/venv/bin:$PATH"

log "llama.cpp $LLAMA_COMMIT"
SRC="$BUILD/llama.cpp"
if [ ! -d "$SRC/.git" ]; then
	git clone -q https://github.com/ggml-org/llama.cpp.git "$SRC"
fi
git -C "$SRC" fetch -q origin
git -C "$SRC" checkout -q -f "$LLAMA_COMMIT"
for p in "$ROOT"/host/metal/patches/*.patch; do
	log "Applying $(basename "$p")"
	git -C "$SRC" apply "$p"
done

# The Metal shader library is embedded in the archive, so there is no
# default.metallib to ship beside the binary.
cmake -S "$SRC" -B "$SRC/build-static" -G Ninja \
	-DGGML_METAL=ON -DGGML_METAL_EMBED_LIBRARY=ON -DGGML_RPC=ON -DGGML_RPC_RDMA=OFF -DGGML_NATIVE=OFF \
	-DBUILD_SHARED_LIBS=OFF -DLLAMA_CURL=OFF -DLLAMA_BUILD_TESTS=OFF \
	-DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_TOOLS=OFF -DLLAMA_BUILD_SERVER=OFF \
	-DCMAKE_OSX_DEPLOYMENT_TARGET="$MACOSX_DEPLOYMENT_TARGET" -DCMAKE_BUILD_TYPE=Release \
	> "$BUILD/ggml-configure.log" 2>&1 || { tail -20 "$BUILD/ggml-configure.log"; exit 1; }
cmake --build "$SRC/build-static" --target ggml ggml-rpc ggml-metal ggml-cpu ggml-blas \
	> "$BUILD/ggml-build.log" 2>&1 || { tail -20 "$BUILD/ggml-build.log"; exit 1; }

log "Collecting into host/out/ggml"
rm -f "$OUT"/*.a
find "$SRC/build-static" -name 'libggml*.a' -exec cp {} "$OUT/" \;
grep -m1 "RPC_PROTO_MAJOR_VERSION" "$SRC/ggml/include/ggml-rpc.h" | tr -s ' ' | cut -d' ' -f3 > "$OUT/rpc-proto-version"
ls -la "$OUT"
