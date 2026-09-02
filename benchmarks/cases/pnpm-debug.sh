#!/bin/sh
# pnpm-install with its errors visible: for hunting a repetition that fails.
set -eu
cd "$WORK/npm"
pnpm install --frozen-lockfile --reporter=append-only --ignore-scripts 2>&1 | grep -vE "^Progress|^Packages|^\+|^\s*$" | tail -60
exit "${PIPESTATUS:-0}"
