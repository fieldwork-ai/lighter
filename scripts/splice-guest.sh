#!/usr/bin/env bash
# Puts the agent (guest/out/lighter-agent) and init (guest/rootfs/init) into a
# copy of a qualified rootfs, and nothing else.
#
#   scripts/splice-guest.sh <qualified rootfs.ext4> <output rootfs.ext4>
#
# Rebuilding the rootfs would move more than the agent: its Dockerfile floats
# `alpine:3.22` and the packages with it, so a release that changes only the
# agent or init takes the last qualified image and replaces those two files.
# The agent is also `lighter-noamd64`, a hard link to the same inode.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE="${1:?usage: $0 <qualified rootfs.ext4> <output rootfs.ext4>}"
OUT="${2:?usage: $0 <qualified rootfs.ext4> <output rootfs.ext4>}"
[ -f "$ROOT/guest/out/lighter-agent" ] || { echo "no guest/out/lighter-agent; run guest/agent/build.sh" >&2; exit 1; }

cp -c "$BASE" "$OUT" 2>/dev/null || cp "$BASE" "$OUT"
WORK="$(mktemp -d "$ROOT/guest/out/splice.XXXX")"
trap 'rm -rf "$WORK"' EXIT
cp "$ROOT/guest/out/lighter-agent" "$WORK/agent"
cp "$ROOT/guest/rootfs/init" "$WORK/init"
cp "$OUT" "$WORK/rootfs.ext4"

docker run --rm -v "$WORK":/w alpine:3.21 sh -c '
set -e
apk add -q e2fsprogs e2fsprogs-extra >/dev/null
cat > /tmp/cmds <<EOF
rm /sbin/lighter-noamd64
rm /sbin/lighter-agent
write /w/agent /sbin/lighter-agent
ln /sbin/lighter-agent /sbin/lighter-noamd64
sif /sbin/lighter-agent links_count 2
sif /sbin/lighter-agent mode 0100755
rm /sbin/lighter-init
write /w/init /sbin/lighter-init
sif /sbin/lighter-init mode 0100755
EOF
debugfs -w -f /tmp/cmds /w/rootfs.ext4 >/dev/null 2>&1
e2fsck -fn /w/rootfs.ext4 >/w/fsck.log 2>&1 || { cat /w/fsck.log; echo "the spliced image does not check clean" >&2; exit 1; }
for f in lighter-agent:agent lighter-noamd64:agent lighter-init:init; do
	debugfs -R "dump /sbin/${f%%:*} /tmp/x" /w/rootfs.ext4 2>/dev/null
	cmp -s /tmp/x "/w/${f#*:}" || { echo "/sbin/${f%%:*} is not what was written" >&2; exit 1; }
done'
cp "$WORK/rootfs.ext4" "$OUT"
echo "==> $OUT: rootfs $(md5 -q "$OUT" | cut -c1-8) from $(md5 -q "$BASE" | cut -c1-8), agent $(md5 -q "$ROOT/guest/out/lighter-agent" | cut -c1-8)"
