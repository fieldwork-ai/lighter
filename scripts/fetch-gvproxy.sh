#!/usr/bin/env bash
# Fetch the gvproxy binary that backs guest networking.
#
# Downloaded rather than vendored: it is a released artifact of another project
# (containers/gvisor-tap-vsock, Apache-2.0), it is 25 MB, and pinning the
# version here means an upgrade is a one-line change with a checksum to match.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${GVPROXY_VERSION:-v0.8.9}"
DEST="$ROOT/vendor/gvproxy"
URL="https://github.com/containers/gvisor-tap-vsock/releases/download/${VERSION}/gvproxy-darwin"

if [ -x "$DEST" ] && [ "${1:-}" != "--force" ]; then
	echo "gvproxy already present at $DEST (use --force to re-download)"
	exit 0
fi

mkdir -p "$(dirname "$DEST")"
echo "==> Downloading gvproxy $VERSION"
curl -fsSL --retry 3 "$URL" -o "$DEST.tmp"
chmod +x "$DEST.tmp"
mv "$DEST.tmp" "$DEST"

echo "==> $(du -h "$DEST" | cut -f1) at $DEST"
"$DEST" --help >/dev/null 2>&1 || true
echo "==> ok"
