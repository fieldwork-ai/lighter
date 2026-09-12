#!/usr/bin/env bash
# The agent runs on Linux; these policy tests also run on macOS CI.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
rustc --edition=2024 --test "$ROOT/guest/agent/src/memory_policy.rs" -o "$WORK/memory-policy"
"$WORK/memory-policy"
