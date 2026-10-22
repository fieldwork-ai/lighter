#!/usr/bin/env bash
# Build a release's guest source archive: the complete corresponding source
# of the Linux guest the release ships, published beside it.
#
#   scripts/guest-source.sh <version>
#
# Writes dist/lighter-<version>-guest-source.tar, holding:
#   linux/   the kernel.org tarball of guest/out/kernel.version (checked
#            against kernel.org's sha256sums), lighter's patches, its
#            configuration and the scripts that build the Image
#   alpine/  Alpine's source package for every package in guest/out/rootfs.ext4,
#            each made by Alpine's own `abuild srcpkg` at the aports commit the
#            installed package records: the upstream source, Alpine's patches
#            and the APKBUILD, with the copyright notices and licences they carry
#
# The GPL asks for the source of what is distributed to go with it, or for a
# written offer to provide it on request; this is the former, so lighter makes
# no offer it would have to keep. Needs docker (a lighter machine is fine) and
# debugfs (brew install e2fsprogs) to read the image's package database.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
VERSION="${1:?usage: $0 <version>}"
NAME="lighter-$VERSION-guest-source"
KERNEL="$(cat guest/out/kernel.version)"
ROOTFS="guest/out/rootfs.ext4"
OUTPUT="dist/$NAME.tar"
CACHE="${LIGHTER_RELEASE_CACHE:-$HOME/Library/Caches/lighter-release}"
# Under the checkout, not /tmp: the Alpine sources are built in a container,
# which sees only the machine's shared folders.
WORK="$ROOT/dist/.$NAME"
rm -rf "$WORK"
mkdir -p "$WORK/$NAME/linux" "$WORK/$NAME/alpine" "$CACHE"
trap 'rm -rf "$WORK"' EXIT

echo "==> Linux $KERNEL"
tarball="linux-$KERNEL.tar.xz"
base="https://cdn.kernel.org/pub/linux/kernel/v${KERNEL%%.*}.x"
if [ ! -f "$CACHE/$tarball" ]; then
	curl -fsSL --retry 3 -o "$CACHE/$tarball.part" "$base/$tarball"
	mv "$CACHE/$tarball.part" "$CACHE/$tarball"
fi
expected="$(curl -fsSL --retry 3 "$base/sha256sums.asc" | awk -v f="$tarball" '$2 == f { print $1 }')"
actual="$(shasum -a 256 "$CACHE/$tarball" | cut -d' ' -f1)"
[ -n "$expected" ] && [ "$actual" = "$expected" ] \
	|| { echo "error: $tarball does not match kernel.org's sha256sums ($actual, expected '$expected')" >&2; exit 1; }
cp "$CACHE/$tarball" "$WORK/$NAME/linux/"
cp -R guest/kernel/patches "$WORK/$NAME/linux/patches"
cp guest/kernel/lighter.config guest/kernel/build.sh guest/kernel/build-kernel.sh guest/kernel/Dockerfile "$WORK/$NAME/linux/"

echo "==> Alpine packages in $ROOTFS"
python3 - "$ROOTFS" "$WORK" <<'PY'
import pathlib, sys
sys.path.insert(0, "scripts")
from notices import alpine_packages
release, packages = alpine_packages(pathlib.Path(sys.argv[1]))
work = pathlib.Path(sys.argv[2])
(work / "alpine-release").write_text(release + "\n")
(work / "origins.tsv").write_text("".join(sorted({f"{p['o']}\t{p['c']}\n" for p in packages})))
(work / "packages.tsv").write_text("package\tversion\tlicence\torigin\taports commit\n"
    + "".join(f"{p['P']}\t{p['V']}\t{p['L']}\t{p['o']}\t{p['c']}\n" for p in packages))
PY
ALPINE="$(cat "$WORK/alpine-release")"
cp "$WORK/packages.tsv" "$WORK/$NAME/alpine/packages.tsv"
cat > "$WORK/srcpkg.sh" <<'SH'
set -eu
apk add -q abuild curl tar >/dev/null
export SRCDEST=/distfiles DISTFILES_MIRROR="https://distfiles.alpinelinux.org/distfiles/v${ALPINE%.*}"
failed=0
while IFS="$(printf '\t')" read -r origin commit; do
	found=
	for repo in main community; do
		rm -rf /build && mkdir -p /build && cd /build
		curl -fsSL --retry 3 "https://gitlab.alpinelinux.org/alpine/aports/-/archive/$commit/aports-$commit.tar.gz?path=$repo/$origin" 2>/dev/null \
			| tar -xz 2>/dev/null || continue
		dir="aports-$commit-$repo-$origin/$repo/$origin"
		[ -f "$dir/APKBUILD" ] || continue
		found=1
		rm -rf "$HOME/packages/src"
		if (cd "$dir" && abuild -F srcpkg >/tmp/srcpkg.log 2>&1) && cp "$HOME"/packages/src/*.src.tar.gz "/work/$NAME/alpine/"; then
			echo "    $repo/$origin"
		else
			echo "error: abuild srcpkg failed for $repo/$origin at $commit" >&2; tail -5 /tmp/srcpkg.log >&2; failed=1
		fi
		break
	done
	[ -n "$found" ] || { echo "error: $origin is in neither main nor community at aports $commit" >&2; failed=1; }
done < /work/origins.tsv
exit "$failed"
SH
docker run --rm -e ALPINE="$ALPINE" -e NAME="$NAME" \
	-v "$WORK:/work" -v lighter-alpine-distfiles:/distfiles \
	"alpine:$ALPINE" sh /work/srcpkg.sh
want="$(wc -l < "$WORK/origins.tsv" | tr -d ' ')"
got="$(find "$WORK/$NAME/alpine" -name '*.src.tar.gz' | wc -l | tr -d ' ')"
[ "$got" = "$want" ] || { echo "error: $got Alpine source packages for $want origins" >&2; exit 1; }

cat > "$WORK/$NAME/README.md" <<EOF
# The source of lighter $VERSION's Linux guest

lighter $VERSION runs a Linux guest made of two parts, both shipped in its release archive: the kernel (\`share/lighter/Image\`) and the root filesystem (\`share/lighter/rootfs.ext4\`). This archive is their complete corresponding source. lighter's own programs in the guest are in lighter's repository, https://github.com/fieldwork-ai/lighter, at the tag v$VERSION, under the MIT License or the Apache License 2.0.

## linux/

Linux $KERNEL as kernel.org publishes it ($tarball, sha256 $actual), the patches lighter applies to it in order (\`patches/\`), its configuration (\`lighter.config\`) and the scripts that build it (\`build.sh\` runs \`build-kernel.sh\` in the container \`Dockerfile\` describes). Linux is licensed under GPL-2.0-only WITH Linux-syscall-note; each patch is under the licence of the files it changes.

## alpine/

The root filesystem is Alpine Linux $ALPINE's packages, listed with their versions, licences and the aports commits they were built from in \`packages.tsv\`. Each \`<origin>-<version>.src.tar.gz\` is Alpine's source package for one origin at one commit (an origin whose packages were built at two commits has two), made by \`abuild srcpkg\` at that commit: the upstream source, Alpine's patches and the APKBUILD that builds them.
EOF

echo "==> Packing $OUTPUT"
COPYFILE_DISABLE=1 tar --uid 0 --gid 0 --uname root --gname root -cf "$OUTPUT" -C "$WORK" "$NAME"
echo "    $OUTPUT ($(du -h "$OUTPUT" | cut -f1)), sha256 $(shasum -a 256 "$OUTPUT" | cut -d' ' -f1)"
