#!/usr/bin/env bash
# Build, sign, notarize and package a release tarball for lighter.
#
#   scripts/package-release.sh <version> [--skip-notarize]
#
# Produces dist/lighter-<version>-arm64.tar.gz containing:
#   bin/lighter             (Developer ID signed with the virtualization entitlement)
#   share/lighter/Image     (guest kernel)
#   share/lighter/rootfs.ext4 (sparse Alpine rootfs)
#   share/lighter/entitlements.plist
#   LICENSE, README.md
#
# The binaries are signed with Apple Developer ID Application and submitted to
# Apple's notarytool so Gatekeeper accepts them without quarantine blocks.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION=""
SKIP_NOTARIZE=""
for arg in "$@"; do
	case "$arg" in
		--skip-notarize) SKIP_NOTARIZE=1 ;;
		-*) echo "unknown option: $arg" >&2; exit 2 ;;
		*) if [ -z "$VERSION" ]; then VERSION="$arg"; else echo "unexpected argument: $arg" >&2; exit 2; fi ;;
	esac
done

[ -n "$VERSION" ] || { echo "usage: $0 <version> [--skip-notarize]" >&2; exit 2; }

if ! command -v cargo >/dev/null 2>&1; then
	# shellcheck disable=SC1091
	[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
fi

WORK="$(mktemp -d)"
STAGE="$WORK/stage/lighter-$VERSION"
mkdir -p "$STAGE/bin" "$STAGE/share/lighter"

# --- credentials -------------------------------------------------------------
CERT_ITEM="y5xjyzmpol4yyhknepjgr3sejm"   # Apple Developer ID Application cert
ASC_ITEM="uepybi2uwfzpv7wlnyoeiuidse"    # App Store Connect API key (notarization)
CACHE="$ROOT/.context"
APP_CACHE="/Users/nick/git/app/.context"

KEYCHAIN=""
ORIGINAL_KEYCHAINS=()
while IFS= read -r kc; do
	[ -n "$kc" ] && ORIGINAL_KEYCHAINS+=("$kc")
done < <(security list-keychains -d user | awk -F'"' '{print $2}' | grep -v '^$' || true)
[ ${#ORIGINAL_KEYCHAINS[@]} -gt 0 ] || ORIGINAL_KEYCHAINS=("$HOME/Library/Keychains/login.keychain-db")

cleanup() {
	if [ -n "$KEYCHAIN" ] && [ -f "$KEYCHAIN" ]; then
		security list-keychains -d user -s "${ORIGINAL_KEYCHAINS[@]}" 2>/dev/null || true
		security delete-keychain "$KEYCHAIN" 2>/dev/null || true
	fi
	rm -rf "$WORK"
}
trap cleanup EXIT

echo "==> Resolving Apple Developer credentials"
if [ -f "$CACHE/cert.p12" ] && [ -f "$CACHE/apple.local.env" ]; then
	echo "    using cached credentials ($CACHE)"
	# shellcheck disable=SC1090
	set -a; source "$CACHE/apple.local.env"; set +a
	cp "$CACHE/cert.p12" "$WORK/cert.p12"
	[ -f "$CACHE/AuthKey.p8" ] && cp "$CACHE/AuthKey.p8" "$WORK/AuthKey.p8" || true
elif [ -f "$APP_CACHE/cert.p12" ] && [ -f "$APP_CACHE/apple.local.env" ]; then
	echo "    using cached credentials ($APP_CACHE)"
	# shellcheck disable=SC1090
	set -a; source "$APP_CACHE/apple.local.env"; set +a
	cp "$APP_CACHE/cert.p12" "$WORK/cert.p12"
	[ -f "$APP_CACHE/AuthKey.p8" ] && cp "$APP_CACHE/AuthKey.p8" "$WORK/AuthKey.p8" || true
else
	echo "    fetching credentials from 1Password"
	cat > "$WORK/creds.tpl" <<TPL
P12_PASSPHRASE={{ op://Shared/$CERT_ITEM/passphrase }}
APPLE_API_KEY_ID={{ op://Shared/$ASC_ITEM/key_id }}
APPLE_API_ISSUER={{ op://Shared/$ASC_ITEM/issuer_id }}
TPL
	op inject -i "$WORK/creds.tpl" -o "$WORK/creds.env" >/dev/null
	# shellcheck disable=SC1090
	set -a; source "$WORK/creds.env"; set +a
	op read "op://Shared/$CERT_ITEM/p12" --out-file "$WORK/cert.p12" >/dev/null
	op read "op://Shared/$ASC_ITEM/p8" --out-file "$WORK/AuthKey.p8" >/dev/null
fi

# Ephemeral keychain for non-interactive codesigning
KEYCHAIN="$WORK/build.keychain"
KEYCHAIN_PASSWORD="$(openssl rand -base64 24)"
security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
security set-keychain-settings -lut 3600 "$KEYCHAIN"
security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
security import "$WORK/cert.p12" -k "$KEYCHAIN" -P "$P12_PASSPHRASE" \
	-T /usr/bin/codesign -T /usr/bin/security >/dev/null
security set-key-partition-list -S apple-tool:,apple:,codesign: \
	-s -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN" >/dev/null
security list-keychains -d user -s "$KEYCHAIN" "${ORIGINAL_KEYCHAINS[@]}"

IDENTITY="$(security find-identity -v -p codesigning "$KEYCHAIN" | awk -F'"' '/Developer ID Application/{print $2; exit}')"
if [ -z "$IDENTITY" ]; then
	echo "error: no Developer ID Application identity found" >&2
	exit 1
fi
echo "==> Signing identity: $IDENTITY"

# --- build -------------------------------------------------------------------
echo "==> Building lighter-cli release binary"
cargo build --release -p lighter-cli

for artifact in guest/out/Image guest/out/rootfs.ext4; do
	[ -f "$artifact" ] || { echo "error: $artifact is missing; run 'make guest'" >&2; exit 1; }
done

cp target/release/lighter "$STAGE/bin/lighter"
cp guest/out/Image guest/out/rootfs.ext4 "$STAGE/share/lighter/"
cp LICENSE README.md "$STAGE/"
cp entitlements.plist "$STAGE/share/lighter/"
# The bundle `lighter start` runs the machine from, shipped rather than
# built on the user's Mac: Gatekeeper assesses an app bundle at first launch,
# and only a notarized one passes without putting a verdict to the user.
APP="$STAGE/share/lighter/lighter.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/lighter "$APP/Contents/MacOS/lighter"
cp assets/Info.plist "$APP/Contents/Info.plist"
cp assets/lighter.icns "$APP/Contents/Resources/lighter.icns"

# --- sign --------------------------------------------------------------------
echo "==> Signing binaries with Developer ID and hardened runtime"
codesign --sign "$IDENTITY" \
	--entitlements entitlements.plist \
	--force \
	--options runtime \
	--timestamp \
	"$STAGE/bin/lighter"

codesign --sign "$IDENTITY" \
	--entitlements entitlements.plist \
	--force \
	--options runtime \
	--timestamp \
	"$APP"

echo "==> Verifying signatures"
codesign --verify --verbose=2 "$STAGE/bin/lighter"
codesign --verify --verbose=2 --deep "$APP"

# --- notarize ----------------------------------------------------------------
if [ -z "$SKIP_NOTARIZE" ] && [ -f "$WORK/AuthKey.p8" ] && [ -n "${APPLE_API_KEY_ID:-}" ]; then
	echo "==> Submitting binaries to Apple Notary Service"
	ZIP="$WORK/notarize.zip"
	mkdir -p "$WORK/notarize"
	cp -R "$STAGE/bin" "$APP" "$WORK/notarize/"
	ditto -c -k --keepParent "$WORK/notarize" "$ZIP"
	xcrun notarytool submit "$ZIP" \
		--key "$WORK/AuthKey.p8" \
		--key-id "$APPLE_API_KEY_ID" \
		--issuer "$APPLE_API_ISSUER" \
		--wait
	echo "==> Notarization accepted by Apple"
	spctl --assess --type execute --verbose=4 "$APP" || true
else
	echo "==> Skipping notarization (--skip-notarize or missing API keys)"
fi

# --- packing -----------------------------------------------------------------
echo "==> Packing release tarball"
mkdir -p dist
TARBALL="dist/lighter-$VERSION-arm64.tar.gz"
TAR_ARGS=("-czf" "$TARBALL" "-C" "$WORK/stage" "lighter-$VERSION")
if tar --help 2>&1 | grep -q -- '--sparse'; then
	TAR_ARGS=("--sparse" "${TAR_ARGS[@]}")
fi
tar "${TAR_ARGS[@]}"

SIZE="$(du -h "$TARBALL" | cut -f1)"
SHA="$(shasum -a 256 "$TARBALL" | cut -d' ' -f1)"

echo
echo "==> Release artifact ready:"
echo "    File:   $TARBALL ($SIZE)"
echo "    SHA256: $SHA"
echo
echo "Formula update (packaging/lighter.rb):"
echo "  url \"https://github.com/fieldwork-ai/lighter/releases/download/v$VERSION/lighter-$VERSION-arm64.tar.gz\""
echo "  sha256 \"$SHA\""
echo "  version \"$VERSION\""
