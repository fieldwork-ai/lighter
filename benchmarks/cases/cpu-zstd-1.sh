#!/bin/sh
# zstd -9 on one thread: the same algorithm and near enough the same code on
# the Mac and in every guest, NEON on both, where sha256 is not (macOS's
# shasum uses the SHA instructions, the guest's sha256sum does not).
set -eu
zstd -q -9 -T1 -c "${TMPDIR:-/tmp}/zstd-corpus" > /dev/null
