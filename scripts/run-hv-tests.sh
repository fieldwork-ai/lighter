#!/usr/bin/env bash
# Run the tests that need a real hypervisor.
#
# `cargo test` cannot run these: the test binary must carry the hypervisor
# entitlement, and signing has to happen between compiling and running. So we
# build without running, sign what cargo produced, then execute it directly.
#
# These are marked `#[ignore]` so a plain `cargo test` stays green on a machine
# that cannot sign — but they cover the virtqueue, which is the code most worth
# testing against real guest memory rather than a mock that could agree with a
# wrong model.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> Building test binaries"
# --message-format=json is how cargo tells us where the binaries landed; the
# paths include a content hash, so guessing them is not an option.
binaries=$(cargo test --no-run --message-format=json "$@" 2>/dev/null \
	| python3 -c '
import json, sys
for line in sys.stdin:
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    if msg.get("reason") != "compiler-artifact" or not msg.get("executable"):
        continue
    # Test harnesses only. Examples also produce executables and would be
    # run as if they were tests, which ends with an example rejecting
    # --ignored as an unknown argument.
    if not msg.get("profile", {}).get("test"):
        continue
    # Our own crates; dependencies have no hypervisor tests.
    if "/lighter" in msg["target"]["src_path"]:
        print(msg["executable"])
')

if [ -z "$binaries" ]; then
	echo "no test binaries were produced" >&2
	exit 1
fi

echo "==> Signing"
# shellcheck disable=SC2086
./scripts/sign.sh $binaries >/dev/null

status=0
for bin in $binaries; do
	name=$(basename "$bin" | sed 's/-[0-9a-f]*$//')
	echo
	echo "==> $name (ignored tests)"
	# --test-threads=1 because these create a VM, and a process may hold
	# exactly one; running them in parallel would fail on HV_BUSY.
	if ! "$bin" --ignored --test-threads=1; then
		status=1
	fi
done

exit $status
