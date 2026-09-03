#!/bin/sh
set -eu
rm -rf "$WORK/npm/node_modules_copy"

# The setup is not the measurement: whatever it queued lands before the
# clock starts, on every runtime alike.
sync
