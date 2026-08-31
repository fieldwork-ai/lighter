#!/bin/sh
# The tree to delete is made fresh each time, and making it is not timed: this
# case is about `rm -rf`, not about the copy.
set -eu
rm -rf "$WORK/npm/doomed"
cp -a "$WORK/npm/node_modules" "$WORK/npm/doomed"
