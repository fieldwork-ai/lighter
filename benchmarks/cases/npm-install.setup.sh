#!/bin/sh
# `npm ci` refuses to run against an existing tree, and removing it is part of
# the scenario rather than part of the measurement.
set -eu
rm -rf "$WORK/npm/node_modules"
