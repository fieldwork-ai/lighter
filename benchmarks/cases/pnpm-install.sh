#!/bin/sh
# The case OrbStack publishes a number for, run the same way, so the two can be
# read against each other. pnpm links from a content-addressed store rather than
# copying, which is why it is worth measuring separately from npm.
set -eu
cd "$WORK/npm"
pnpm install --frozen-lockfile --reporter=silent --ignore-scripts
