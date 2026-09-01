#!/usr/bin/env bash
# Build a release tarball: the binaries, the guest, and the entitlement.
#
#   scripts/package-release.sh 0.1.0
#
# Produces dist/lighter-<version>-arm64.tar.gz containing everything an
# installed copy needs. The guest root filesystem is sparse — two gigabytes
# logical, a couple of hundred megabytes of actual content — so the tarball is
# built with `--sparse` and is a fraction of what `ls` suggests.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="${1:-}"
[ -n "$VERSION" ] || { echo "usage: $0 <version>" >&2; exit 2; }

if ! command -v cargo >/dev/null 2>&1; then
	# shellcheck disable=SC1091
	. "$HOME/.cargo/env"
fi

STAGE="dist/lighter-$VERSION"
rm -rf "$STAGE"
mkdir -p "$STAGE/bin" "$STAGE/share/lighter"

echo "==> Building"
cargo build --release -p lighter-cli
[ -x vendor/gvproxy ] || ./scripts/fetch-gvproxy.sh
for artifact in guest/out/Image guest/out/rootfs.ext4; do
	[ -f "$artifact" ] || { echo "$artifact is missing; run 'make guest'" >&2; exit 1; }
done

cp target/release/lighter "$STAGE/bin/lighter"
cp vendor/gvproxy "$STAGE/bin/gvproxy"
cp guest/out/Image guest/out/rootfs.ext4 "$STAGE/share/lighter/"
cp LICENSE README.md "$STAGE/"
# The entitlement travels with the binary: Homebrew installs unsigned files, so
# the formula signs on the way in and needs this to sign against.
cp entitlements.plist "$STAGE/share/lighter/"

echo "==> Packing"
mkdir -p dist
tar --sparse -czf "dist/lighter-$VERSION-arm64.tar.gz" -C dist "lighter-$VERSION"
rm -rf "$STAGE"

SIZE="$(du -h "dist/lighter-$VERSION-arm64.tar.gz" | cut -f1)"
SHA="$(shasum -a 256 "dist/lighter-$VERSION-arm64.tar.gz" | cut -d' ' -f1)"
echo
echo "==> dist/lighter-$VERSION-arm64.tar.gz ($SIZE)"
echo "    sha256 $SHA"
echo
echo "Put that sha256 in the Homebrew formula (packaging/lighter.rb)."
