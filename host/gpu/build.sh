#!/usr/bin/env bash
# Build the GPU renderer lighter links statically: virglrenderer's Venus
# renderer over MoltenVK. Output: host/out/{libvirglrenderer,libvirgl,libmesa,libMoltenVK}.a
#
# Needs only the Command Line Tools and python3: meson, ninja and mako come
# from a venv under host/build, MoltenVK from the Khronos release tarball,
# virglrenderer from its git at a pinned commit with host/gpu/patches applied.
# There is no Homebrew here on purpose — the release machine has none.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUILD="$ROOT/host/build"
OUT="$ROOT/host/out"
MVK_VERSION="${MVK_VERSION:-1.4.2}"
MVK_SHA256="${MVK_SHA256:-}"
VIRGL_COMMIT="${VIRGL_COMMIT:-32dac0c}"
# lighter's floor; objects built for newer are a warning at every link.
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-15.0}"
mkdir -p "$BUILD" "$OUT"

log() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

log "Python tools (meson, ninja, mako)"
if [ ! -x "$BUILD/venv/bin/meson" ]; then
	python3 -m venv "$BUILD/venv"
	"$BUILD/venv/bin/pip" install -q --upgrade pip
	"$BUILD/venv/bin/pip" install -q meson ninja mako
fi
export PATH="$BUILD/venv/bin:$BUILD/bin:$PATH"

log "MoltenVK $MVK_VERSION"
MVK="$BUILD/MoltenVK-$MVK_VERSION"
if [ ! -f "$MVK/MoltenVK/static/MoltenVK.xcframework/macos-arm64_x86_64/libMoltenVK.a" ]; then
	tar="$BUILD/MoltenVK-macos-$MVK_VERSION.tar"
	curl -fsSL -o "$tar" "https://github.com/KhronosGroup/MoltenVK/releases/download/v$MVK_VERSION/MoltenVK-macos.tar"
	if [ -n "$MVK_SHA256" ]; then
		echo "$MVK_SHA256  $tar" | shasum -a 256 -c - >/dev/null
	fi
	rm -rf "$MVK" && mkdir -p "$MVK" && tar xf "$tar" -C "$MVK"
fi
MVK_INC="$MVK/MoltenVK/include"
MVK_LIB="$MVK/MoltenVK/static/MoltenVK.xcframework/macos-arm64_x86_64"

# meson wants a pkg-config; there is none without Homebrew, and the only
# module anyone asks for is vulkan, which is MoltenVK.
mkdir -p "$BUILD/bin"
cat > "$BUILD/bin/pkg-config" <<PC
#!/bin/sh
case "\$*" in
  *--version*) echo 1.9.5;;
  *--modversion*vulkan*) echo $MVK_VERSION;;
  *--cflags*vulkan*) echo "-I$MVK_INC";;
  *--libs*vulkan*) echo "-L$MVK_LIB -lMoltenVK -framework Metal -framework Foundation -framework IOSurface -framework QuartzCore -framework IOKit -framework CoreGraphics -framework AppKit -lc++";;
  *--exists*vulkan*|*--atleast-version*) exit 0;;
  *--print-variables*|*--variable*) exit 0;;
  *) exit 1;;
esac
PC
chmod +x "$BUILD/bin/pkg-config"

log "virglrenderer $VIRGL_COMMIT"
SRC="$BUILD/virglrenderer"
if [ ! -d "$SRC/.git" ]; then
	git clone -q https://gitlab.freedesktop.org/virgl/virglrenderer.git "$SRC"
fi
git -C "$SRC" fetch -q origin
git -C "$SRC" checkout -q -f "$VIRGL_COMMIT"
git -C "$SRC" clean -qfdx -e build
for p in "$ROOT"/host/gpu/patches/*.patch; do
	echo "  applying $(basename "$p")"
	patch -d "$SRC" -p1 --silent < "$p"
done
# The Metal helper includes the protocol's header by a path the subproject
# does not lay out; give it that path.
meson setup "$SRC/build" "$SRC" --reconfigure \
	-Dvrend=false -Dvenus=true \
	-Drender-server-mode=thread -Drender-server-worker=thread \
	-Dvulkan-dload=false -Ddefault_library=static -Dtests=false \
	-Dc_args="-mmacosx-version-min=$MACOSX_DEPLOYMENT_TARGET" \
	-Dobjc_args="-mmacosx-version-min=$MACOSX_DEPLOYMENT_TARGET" >"$BUILD/meson-setup.log" 2>&1 \
	|| { tail -20 "$BUILD/meson-setup.log"; exit 1; }
vp="$(ls -d "$SRC"/subprojects/venus-protocol-*/ | head -1)"
ln -sfn include/vulkan "$vp/venus-protocol"
ninja -C "$SRC/build" >"$BUILD/ninja.log" 2>&1 || { tail -20 "$BUILD/ninja.log"; exit 1; }

log "Collecting into host/out"
cp "$SRC/build/src/libvirglrenderer.a" "$SRC/build/src/libvirgl.a" "$SRC/build/src/mesa/libmesa.a" "$MVK_LIB/libMoltenVK.a" "$OUT/"
ls -la "$OUT"
