#!/bin/sh
# Every core at once: eight sha256 streams of 256 MiB side by side, the
# suite's vCPU count. cpu-sha256 is one core; this is the machine.
set -eu
sum() { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi; }
i=0
while [ "$i" -lt 8 ]; do
	head -c 268435456 /dev/zero | sum > /dev/null &
	i=$((i + 1))
done
wait
