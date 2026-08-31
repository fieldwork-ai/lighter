#!/bin/sh
# Pure metadata: no file is opened and no byte is read. This isolates LOOKUP and
# READDIR, which is where a shared filesystem is slowest and where caching has
# the most to give.
set -eu
cd "$WORK/npm"
find node_modules -type f | wc -l > /dev/null
