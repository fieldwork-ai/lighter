#!/usr/bin/env bash
# Exercise the real installer with a local archive and isolated PATH targets.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d -t lighter-install-test)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/local/bin" "$WORK/home" "$WORK/archive/lighter-test/bin" "$WORK/archive/lighter-test/share"
printf '#!/bin/sh\nexit 0\n' > "$WORK/archive/lighter-test/bin/lighter"
tar -czf "$WORK/release.tar.gz" -C "$WORK/archive" lighter-test
# Redirect the system PATH directory in this copy; never touch a real install.
sed "s|/usr/local/bin|$WORK/local/bin|g" "$ROOT/scripts/install.sh" > "$WORK/install.sh"
install_into() {
	HOME="$WORK/home" LIGHTER_INSTALL_DIR="$WORK/$1" LIGHTER_VERSION=test \
		LIGHTER_TARBALL_URL="file://$WORK/release.tar.gz" GITHUB_TOKEN= \
		bash "$WORK/install.sh" > "$WORK/output" 2>&1
}
TARGET="$WORK/local/bin/lighter"
printf '#!/bin/sh\nexec /checkout/target/release/lighter "$@"\n' > "$TARGET"
cp "$TARGET" "$WORK/wrapper"
for prefix in first second; do
	install_into "$prefix"
	cmp "$TARGET" "$WORK/wrapper"
	[ ! -L "$TARGET" ]
	[ -x "$WORK/$prefix/bin/lighter" ]
	grep -Fq "\"$WORK/$prefix/bin/lighter\" start" "$WORK/output"
done
rm "$TARGET"
for prefix in first second; do
	install_into "$prefix"
	[ "$(readlink "$TARGET")" = "$WORK/$prefix/bin/lighter" ]
done
ln -sfn "$WORK/missing" "$TARGET"
install_into first
[ "$(readlink "$TARGET")" = "$WORK/first/bin/lighter" ]
rm "$TARGET"
mkdir "$TARGET"
install_into second
[ -d "$TARGET" ] && [ ! -L "$TARGET" ] && [ ! -e "$TARGET/lighter" ]
echo 'installer: wrapper preserved across two prefixes; symlinks updated; directory preserved'
