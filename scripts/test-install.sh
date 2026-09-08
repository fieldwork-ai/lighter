#!/usr/bin/env bash
# Exercise the real installer with a local archive and isolated PATH targets.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d -t lighter-install-test)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/local/bin" "$WORK/home" "$WORK/archive/lighter-test/bin" "$WORK/archive/lighter-test/share"
printf '#!/bin/sh\nexit 0\n' > "$WORK/archive/lighter-test/bin/lighter"
tar -czf "$WORK/release.tar.gz" -C "$WORK/archive" lighter-test
# Isolate the signature verifier and bootstrap only in this test copy. The
# production installer has no bypass. Real artifact trust is tested separately.
cat > "$WORK/bootstrap" <<'BOOT'
#!/bin/bash
set -eu
[ "$1" = install-archive ]; shift
while [ "$#" -gt 0 ]; do
  case "$1" in
    --archive) archive="$2"; shift 2 ;;
    --prefix) prefix="$2"; shift 2 ;;
    --restart) shift ;;
    *) exit 2 ;;
  esac
done
mkdir -p "$prefix"
tar -xzf "$archive" --strip-components=1 -C "$prefix"
BOOT
chmod +x "$WORK/bootstrap"
sed -e "s|/usr/local/bin|$WORK/local/bin|g" -e 's|/usr/bin/codesign|/usr/bin/true|g' "$ROOT/scripts/install.sh" > "$WORK/install.sh"
install_into() {
	HOME="$WORK/home" LIGHTER_INSTALL_DIR="$WORK/$1" LIGHTER_VERSION=test \
		LIGHTER_TARBALL_URL="file://$WORK/release.tar.gz" LIGHTER_BOOTSTRAP_URL="file://$WORK/bootstrap" GITHUB_TOKEN= \
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

# Invalid bootstrap identity must fail before invoking the helper or touching
# any existing installation. No shell archive extraction precedes validation.
sed -e "s|/usr/local/bin|$WORK/local/bin|g" -e 's|/usr/bin/codesign|/usr/bin/false|g' "$ROOT/scripts/install.sh" > "$WORK/install.sh"
if install_into rejected; then echo 'invalid bootstrap accepted' >&2; exit 1; fi
[ ! -e "$WORK/rejected/bin/lighter" ]
echo 'installer: invalid bootstrap rejected before installation'
