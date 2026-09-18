#!/usr/bin/env bash
# The agent runs on Linux; the idle pass's configuration is plain std and tests anywhere.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
rustc --edition=2024 --test "$ROOT/guest/agent/src/idle.rs" -o "$WORK/idle"
"$WORK/idle"
