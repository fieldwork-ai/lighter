#!/bin/sh
# zstd -3 on eight threads, the suite's vCPU count.
set -eu
zstd -q -3 -T8 -c "${TMPDIR:-/tmp}/zstd-corpus" > /dev/null
