#!/bin/sh
# An npm install of a pinned tree: the canonical shared-filesystem workload.
#
# `npm ci` rather than `npm install` because it is deterministic — it installs
# exactly the lockfile, with no resolution step whose duration depends on the
# network. What is left is what we want to measure: several hundred thousand
# creates, writes, renames and stats, almost all of them on small files.
set -eu
cd "$WORK/npm"
npm ci --no-audit --no-fund --loglevel=error --prefer-offline
