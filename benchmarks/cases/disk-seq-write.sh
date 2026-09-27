#!/bin/sh
# Bandwidth, not operations: one 1 GiB file written sequentially in 1 MiB
# blocks and fsynced, so the number is what reached the Mac's disk (the host
# folder) or the runtime's own (--where guest), not what sat in a page cache.
# macOS's dd has no conv=fsync; the native baseline syncs after it instead.
set -eu
if ! dd if=/dev/zero of="$WORK/seq.bin" bs=1048576 count=1024 conv=fsync 2>/dev/null; then
	dd if=/dev/zero of="$WORK/seq.bin" bs=1048576 count=1024 2>/dev/null
	sync
fi
rm -f "$WORK/seq.bin"
