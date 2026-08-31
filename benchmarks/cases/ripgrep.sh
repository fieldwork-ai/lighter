#!/bin/sh
# Reading every file in a large tree: the other half of the workload. An install
# is write-heavy and metadata-heavy; this is read-heavy and lookup-heavy, and a
# cache that helps one can easily not help the other.
set -eu
cd "$WORK/npm"
rg --no-messages --count-matches 'function' node_modules > /dev/null || true
