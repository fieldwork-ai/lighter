#!/bin/sh
# The lockfiles come back before every run: `npm ci` rewrites any yarn.lock it
# finds beside a package-lock.json, so without this the yarn case measures
# whichever cases happened to run before it — or fails outright.
set -eu
cp "$WORK"/fixture/* "$WORK/npm/"
. "$WORK/cases/clear.sh"
clear_tree "$WORK/npm/node_modules"

# The setup is not the measurement: whatever it queued lands before the
# clock starts, on every runtime alike.
sync
