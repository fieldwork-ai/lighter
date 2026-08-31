#!/bin/sh
# Deleting a package tree: sixty thousand unlinks and four thousand rmdirs, and
# nothing else. OrbStack publishes a figure for this one too.
set -eu
rm -rf "$WORK/npm/doomed"
