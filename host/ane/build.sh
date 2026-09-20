#!/usr/bin/env bash
# Build ONNX Runtime with its CoreML provider as static archives lighter links:
# every lib*.a the build produces, collected into host/out/ort. Needs the
# Command Line Tools and python3; cmake and ninja come from a venv. Long: the
# better part of an hour on an M1.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUILD="$ROOT/host/build"
OUT="$ROOT/host/out/ort"
ORT_VERSION="${ORT_VERSION:-1.30.0}"
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

log "ONNX Runtime v$ORT_VERSION"
SRC="$BUILD/onnxruntime"
if [ ! -d "$SRC/.git" ]; then
	git clone -q --depth 1 --branch "v$ORT_VERSION" https://github.com/microsoft/onnxruntime.git "$SRC"
fi
# A stale Homebrew prefix on the machine must not be found (its abseil breaks
# the configure); the policy floor is for CMake 4 against old dependencies.
python3 "$SRC/tools/ci_build/build.py" \
	--build_dir "$SRC/build" --config Release --parallel "$(sysctl -n hw.ncpu)" \
	--use_coreml --skip_tests --skip_submodule_sync --cmake_generator Ninja \
	--cmake_extra_defines onnxruntime_BUILD_SHARED_LIB=OFF \
		onnxruntime_BUILD_UNIT_TESTS=OFF \
		CMAKE_OSX_DEPLOYMENT_TARGET="$MACOSX_DEPLOYMENT_TARGET" \
		CMAKE_IGNORE_PREFIX_PATH=/opt/homebrew \
		CMAKE_POLICY_VERSION_MINIMUM=3.5 \
	> "$BUILD/ort-build.log" 2>&1 || { tail -30 "$BUILD/ort-build.log"; exit 1; }

# build.py builds ONNX Runtime's own targets; re2, which the session code
# links, is a dependency that its static build leaves unbuilt on some
# machines (an M5 Pro; an M1 built it). Built by name.
ninja -C "$SRC/build/Release" re2 > "$BUILD/ort-re2.log" 2>&1 || { tail -5 "$BUILD/ort-re2.log"; exit 1; }

log "Collecting archives into host/out/ort"
rm -f "$OUT"/*.a
find "$SRC/build/Release" -name '*.a' \
	-not -name '*test*' -not -name '*mock*' -not -name '*benchmark*' \
	-exec cp {} "$OUT/" \;
ls "$OUT" | wc -l | xargs echo "  archives:"
