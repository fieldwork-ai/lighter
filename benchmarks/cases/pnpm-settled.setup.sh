#!/bin/sh
# pnpm-install's setup, then a pause for the filesystem underneath to finish
# what the removal started: an experiment in whether the spread between
# repetitions is the previous tree's deletion still being reclaimed.
set -eu
cp "$WORK"/fixture/* "$WORK/npm/"
. "$WORK/cases/clear.sh"
clear_tree "$WORK/npm/node_modules"
sync
sleep 3
