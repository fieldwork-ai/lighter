#!/bin/sh
# The other install OrbStack publishes. Classic yarn, because that is what the
# comparison was made with and what most projects still have a lockfile for.
set -eu
cd "$WORK/npm"
yarn install --frozen-lockfile --non-interactive --no-progress --silent --ignore-scripts
