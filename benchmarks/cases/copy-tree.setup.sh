#!/bin/sh
set -eu
. "$WORK/cases/clear.sh"
clear_tree "$WORK/npm/node_modules_copy"

# The setup is not the measurement: whatever it queued lands before the
# clock starts, on every runtime alike.
sync
