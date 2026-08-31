#!/usr/bin/env bash
# Sign a binary with the hypervisor entitlement.
#
# Hypervisor.framework refuses every call from an unsigned binary, so this runs
# after each build rather than at release time only. Ad-hoc signing (`-`) is
# enough for a binary that never leaves this machine; releases pass
# LIGHTER_SIGN_IDENTITY to use a real Developer ID.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IDENTITY="${LIGHTER_SIGN_IDENTITY:--}"

if [ $# -eq 0 ]; then
	echo "usage: sign.sh <binary> [binary...]" >&2
	exit 2
fi

for bin in "$@"; do
	if [ ! -f "$bin" ]; then
		echo "sign.sh: no such binary: $bin" >&2
		exit 1
	fi
	codesign --sign "$IDENTITY" \
		--entitlements "$ROOT/entitlements.plist" \
		--force \
		--timestamp=none \
		"$bin" 2>&1 | sed "s|^|  |" || {
		echo "sign.sh: failed to sign $bin" >&2
		exit 1
	}
done
