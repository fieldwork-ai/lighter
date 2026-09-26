#!/bin/sh
# zstd -9 on eight threads, the suite's vCPU count.
set -eu
zstd -q -9 -T8 -c "${TMPDIR:-/tmp}/zstd-corpus" > /dev/null
