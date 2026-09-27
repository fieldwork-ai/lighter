#!/usr/bin/env bash
# Private tools for comparable native/container measurements. No global installs.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TOOLS="$ROOT/.logs/050/tools/native"
ARCHIVE="$ROOT/.logs/050/tools/node-v24.18.0-darwin-arm64.tar.gz"
mkdir -p "$(dirname "$ARCHIVE")"
if [ ! -f "$ARCHIVE" ]; then
	curl -fL https://nodejs.org/dist/v24.18.0/node-v24.18.0-darwin-arm64.tar.gz -o "$ARCHIVE.part"
	mv "$ARCHIVE.part" "$ARCHIVE"
fi
[ "$(shasum -a 256 "$ARCHIVE" | awk '{print $1}')" = e1a97e14c99c803e96c7339403282ea05a499c32f8d83defe9ef5ec66f979ed1 ]
if [ ! -x "$TOOLS/bin/node" ]; then
	mkdir -p "$TOOLS"
	tar -xzf "$ARCHIVE" --strip-components=1 -C "$TOOLS"
fi
export PATH="$TOOLS/bin:$PATH"
if [ ! -x "$TOOLS/bin/pnpm" ] || [ ! -x "$TOOLS/bin/yarn" ] || [ "$(npm --version)" != 11.16.0 ] || [ "$(pnpm --version 2>/dev/null || true)" != 10.28.0 ] || [ "$(yarn --version 2>/dev/null || true)" != 1.22.22 ]; then
	npm --prefix "$TOOLS" --cache "$TOOLS/install-cache" install -g --force --no-audit --no-fund npm@11.16.0 pnpm@10.28.0 yarn@1.22.22
fi
python3 - "$ROOT/benchmarks/toolchain.json" "$TOOLS/versions.json" <<'PY'
import json, pathlib, subprocess, sys
expected = json.loads(pathlib.Path(sys.argv[1]).read_text())
actual = {name: subprocess.check_output([name, '--version'], text=True).strip()
          for name in ['node', 'npm', 'pnpm', 'yarn']}
assert all(actual[name] == expected[name] for name in actual), actual
pathlib.Path(sys.argv[2]).write_text(json.dumps(actual, indent=2) + '\n')
print(json.dumps(actual))
PY

# llama.cpp and zstd for the media cases, built here rather than taken from
# Homebrew: its llama.cpp is another release and uses Accelerate's BLAS for the
# prompt, which the image does not have, so a native row would compare builds.
# The CPU build is the image's release (benchmarks/Dockerfile) with the image's
# flags: no BLAS, no Metal, armv8.2-a+dotprod, OpenMP threads (Homebrew's
# libomp). The Metal build is the commit lighter's Metal server is built from
# (host/metal/build.sh), for llm-gpu.sh, into tools/native/metal.
LLAMA_TAG="$(sed -n 's/.*--branch \(b[0-9]*\) .*llama.cpp.*/\1/p' "$ROOT/benchmarks/Dockerfile")"
LLAMA_METAL="$(sed -n 's/^LLAMA_COMMIT="${LLAMA_COMMIT:-\([0-9a-f]*\)}"/\1/p' "$ROOT/host/metal/build.sh")"
ZSTD="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["zstd"])' "$ROOT/benchmarks/toolchain.json")"
build_llama() { # dest, ref, cmake arguments...
	local dest="$1" ref="$2"; shift 2
	[ "$(cat "$dest/llama.ref" 2>/dev/null)" = "$ref" ] && [ -x "$dest/bin/llama-bench" ] && return
	local src="$TOOLS/src/llama-$ref"
	rm -rf "$src"; mkdir -p "$TOOLS/src" "$dest/bin"
	git clone -q https://github.com/ggml-org/llama.cpp "$src" && git -C "$src" checkout -q "$ref"
	cmake -S "$src" -B "$src/build" -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF \
		-DLLAMA_CURL=OFF -DLLAMA_BUILD_SERVER=OFF "$@" > "$TOOLS/src/llama-$ref-configure.log"
	cmake --build "$src/build" --target llama-bench -j > "$TOOLS/src/llama-$ref-build.log"
	cp "$src/build/bin/llama-bench" "$dest/bin/"
	echo "$ref" > "$dest/llama.ref"
	rm -rf "$src"
}
LIBOMP="$(brew --prefix libomp 2>/dev/null || true)"
[ -d "$LIBOMP" ] || { echo "the image's llama.cpp threads with OpenMP: brew install libomp" >&2; exit 1; }
build_llama "$TOOLS" "$LLAMA_TAG" -DGGML_METAL=OFF -DGGML_BLAS=OFF -DGGML_NATIVE=OFF \
	-DGGML_CPU_ARM_ARCH=armv8.2-a+dotprod -DGGML_OPENMP=ON -DOpenMP_ROOT="$LIBOMP"
otool -L "$TOOLS/bin/llama-bench" | grep -q libomp || { echo "llama-bench did not link OpenMP" >&2; exit 1; }
build_llama "$TOOLS/metal" "$LLAMA_METAL" -DGGML_METAL=ON
if [ "$("$TOOLS/bin/zstd" --version 2>/dev/null | sed -n 's/.* v\([0-9.]*\),.*/\1/p')" != "$ZSTD" ]; then
	rm -rf "$TOOLS/src/zstd"
	git clone -q --depth 1 --branch "v$ZSTD" https://github.com/facebook/zstd "$TOOLS/src/zstd"
	make -C "$TOOLS/src/zstd/programs" -j zstd > "$TOOLS/src/zstd-build.log"
	cp "$TOOLS/src/zstd/programs/zstd" "$TOOLS/bin/zstd"
	rm -rf "$TOOLS/src/zstd"
fi
echo "llama.cpp: $("$TOOLS/bin/llama-bench" --version 2>&1 | grep -m1 version); metal: $("$TOOLS/metal/bin/llama-bench" --version 2>&1 | grep -m1 version)"
echo "zstd: $("$TOOLS/bin/zstd" --version)"
