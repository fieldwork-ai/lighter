#!/usr/bin/env bash
# Milestone 3 gate, part three: the network under load.
#
# m3-network.sh proves the guest is on the network; this proves it stays
# there. Twenty seconds of iperf3 on each of four paths, then the two checks
# that matter: no RCU stall in the guest's log, and a Docker socket that
# still answers. The first device blocked a vCPU thread on gvproxy's socket
# and failed both at 1.5 Gbit/s.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
if ! command -v cargo >/dev/null 2>&1; then
	# shellcheck disable=SC1091
	. "$HOME/.cargo/env"
fi
echo "==> Building and signing the CLI"
cargo build --release -p lighter-cli >/dev/null
./scripts/sign.sh target/release/lighter >/dev/null
echo "==> Four paths, 20 s each, then the stall and socket checks"
scripts/capped.sh 240 scripts/net-iperf.sh --seconds 20
