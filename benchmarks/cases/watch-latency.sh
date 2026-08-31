#!/bin/sh
# How long after the host changes a file the target can see the change.
#
# Measured as a full round trip on one clock — the target writes a request, the
# host answers it, and the target waits for the answer — because the two sides
# have no clock they agree on to milliseconds. That makes the number strictly
# larger than the one being claimed, which is the right direction for a
# benchmark to be wrong in.
set -eu
. "$WORK/cases/lib.sh"
cd "$WORK"

round=0
setup() { round=$((round + 1)); printf '%s' "$round" > request; }
body() {
	waited=0
	while [ "$(cat reply 2>/dev/null || echo)" != "$round" ]; do
		waited=$((waited + 1))
		if [ "$waited" -gt 200000 ]; then
			echo "WATCH round $round never answered" >&2
			exit 1
		fi
	done
}
repeat
