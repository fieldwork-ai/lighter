#!/usr/bin/env bash
# The agent runs on Linux; the warm loop's arithmetic and its loop over plain files test anywhere.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
rustc --edition=2024 --test "$ROOT/guest/agent/src/warm.rs" -o "$WORK/warm"
"$WORK/warm"
