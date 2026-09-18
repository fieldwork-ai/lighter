#!/usr/bin/env bash
# The agent runs on Linux; the inbound fallback rule is plain std and tests anywhere.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
rustc --edition=2024 --test "$ROOT/guest/agent/src/inbound.rs" -o "$WORK/inbound"
"$WORK/inbound"
