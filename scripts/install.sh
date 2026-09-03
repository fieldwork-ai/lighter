#!/usr/bin/env bash
# One-line installer for lighter.
#
#   curl -fsSL https://raw.githubusercontent.com/fieldwork-ai/lighter/main/scripts/install.sh | sh
#
# Requirements:
#   - Apple Silicon Mac (arm64)
#   - macOS 15 (Sequoia) or later
set -euo pipefail

log() { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
err() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# 1. System checks
OS="$(uname -s)"
[ "$OS" = "Darwin" ] || err "lighter runs only on macOS (found $OS)"

ARCH="$(uname -m)"
[ "$ARCH" = "arm64" ] || err "lighter requires Apple Silicon (arm64, found $ARCH). Intel is not supported."

MACOS_VER="$(sw_vers -productVersion)"
MACOS_MAJOR="${MACOS_VER%%.*}"
if [ "$MACOS_MAJOR" -lt 15 ]; then
	err "lighter requires macOS 15 (Sequoia) or later (found $MACOS_VER)"
fi

# 2. Release resolution
REPO="fieldwork-ai/lighter"
VERSION="${LIGHTER_VERSION:-}"
URL="${LIGHTER_TARBALL_URL:-}"

if [ -z "$URL" ]; then
	if [ -z "$VERSION" ]; then
		AUTH_HEADER=()
		[ -n "${GITHUB_TOKEN:-}" ] && AUTH_HEADER=(-H "Authorization: Bearer $GITHUB_TOKEN")
		LATEST_JSON="$(curl -fsSL "${AUTH_HEADER[@]}" -H "Accept: application/vnd.github+json" "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null || true)"
		TAG="$(printf '%s\n' "$LATEST_JSON" | grep '"tag_name":' | head -1 | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/' || true)"
		if [ -z "$TAG" ]; then
			err "could not find the latest release on GitHub. Set LIGHTER_VERSION=<version> to specify manually."
		fi
		VERSION="${TAG#v}"
	fi
	TAG="v$VERSION"
	URL="https://github.com/$REPO/releases/download/$TAG/lighter-$VERSION-arm64.tar.gz"
	# A private repository answers the download URL with 404 whatever the
	# token; its assets are fetched through the API by id. With a token in
	# hand, resolve the asset that way so an install works before the
	# repository is public, and for a fork that never will be.
	if [ -n "${GITHUB_TOKEN:-}" ]; then
		ASSET_ID="$(curl -fsSL -H "Authorization: Bearer $GITHUB_TOKEN" -H "Accept: application/vnd.github+json" "https://api.github.com/repos/$REPO/releases/tags/$TAG" 2>/dev/null \
			| tr -d '\n' | grep -oE '"id": *[0-9]+,[^}]*"name": *"lighter-'"$VERSION"'-arm64\.tar\.gz"' | head -1 | grep -oE '[0-9]+' | head -1 || true)"
		[ -n "$ASSET_ID" ] && URL="https://api.github.com/repos/$REPO/releases/assets/$ASSET_ID"
	fi
fi

log "Installing lighter $VERSION"

# 3. Download & Extract
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

TARBALL="$WORK/lighter.tar.gz"
log "Downloading $URL"
DOWNLOAD_HEADERS=()
[ -n "${GITHUB_TOKEN:-}" ] && DOWNLOAD_HEADERS=(-H "Authorization: Bearer $GITHUB_TOKEN" -H "Accept: application/octet-stream")
curl -fL --progress-bar "${DOWNLOAD_HEADERS[@]}" "$URL" -o "$TARBALL" || err "failed to download release archive from $URL"

INSTALL_DIR="${LIGHTER_INSTALL_DIR:-$HOME/.lighter}"
mkdir -p "$INSTALL_DIR"

log "Extracting into $INSTALL_DIR"
tar -xzf "$TARBALL" --strip-components=1 -C "$INSTALL_DIR"

# Clean any quarantine attributes inherited from download
xattr -dr com.apple.quarantine "$INSTALL_DIR/bin" "$INSTALL_DIR/share" 2>/dev/null || true

# 4. Symlink to PATH
BIN="$INSTALL_DIR/bin/lighter"
chmod +x "$BIN" "$INSTALL_DIR/bin/gvproxy"

TARGET_BIN=""
if [ -w "/usr/local/bin" ]; then
	TARGET_BIN="/usr/local/bin/lighter"
elif mkdir -p "$HOME/.local/bin" 2>/dev/null && [ -w "$HOME/.local/bin" ]; then
	TARGET_BIN="$HOME/.local/bin/lighter"
fi

if [ -n "$TARGET_BIN" ]; then
	ln -sf "$BIN" "$TARGET_BIN"
	log "Linked $BIN -> $TARGET_BIN"
else
	TARGET_BIN="$BIN"
	warn "Could not write to /usr/local/bin or ~/.local/bin."
	warn "Add $INSTALL_DIR/bin to your PATH to run 'lighter' directly."
fi

# 5. Check PATH
case ":$PATH:" in
	*":$(dirname "$TARGET_BIN"):*"|*":$INSTALL_DIR/bin:*") ;;
	*)
		warn "$(dirname "$TARGET_BIN") is not in your PATH."
		warn "Add it to your shell configuration (e.g. ~/.zshrc):"
		warn "  export PATH=\"$(dirname "$TARGET_BIN"):\$PATH\""
		;;
esac

# 6. Verification
log "Verifying installation"
if "$BIN" doctor >/dev/null 2>&1; then
	printf '\n\033[1;32m✓\033[0m lighter %s installed successfully!\n\n' "$VERSION"
else
	printf '\n\033[1;32m✓\033[0m lighter %s installed.\n\n' "$VERSION"
	"$BIN" doctor || true
fi

echo "Start the daemon:"
echo "  lighter start"
echo
echo "Or start automatically on login:"
echo "  lighter install"
echo
echo "Point your Docker CLI at lighter:"
echo "  docker ps"
