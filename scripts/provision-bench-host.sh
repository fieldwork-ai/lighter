#!/usr/bin/env bash
# Prepare another Mac to run the benchmarks.
#
#   scripts/provision-bench-host.sh admin@192.168.50.21
#
# A benchmark host has to be quiet, and the machine you work on is not. This
# sets up a second one: toolchain, the fixture's dependencies, the repository,
# and the guest artifacts — which it builds *there*, using whatever container
# runtime the host already has, rather than copying a two-gigabyte root
# filesystem over the network.
#
# It is deliberately not idempotent in the clever sense: every step checks for
# what it needs and skips if it is present, so running it again after a failure
# picks up where it stopped.
set -euo pipefail

HOST="${1:-}"
[ -n "$HOST" ] || { echo "usage: $0 user@host" >&2; exit 2; }
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REMOTE_DIR="${REMOTE_DIR:-lighter}"

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
remote() { ssh -o ConnectTimeout=10 "$HOST" "$@"; }

step "Checking the host"
remote 'sw_vers -productVersion; sysctl -n hw.model hw.ncpu; echo "$(( $(sysctl -n hw.memsize) / 1024 / 1024 )) MiB"'
remote 'xcode-select -p >/dev/null 2>&1' \
	|| { echo "the command line tools are missing; run: xcode-select --install" >&2; exit 1; }

step "Toolchain"
# Homebrew first, since everything else comes through it. `brew` is not on a
# non-interactive PATH, so every later command names it explicitly.
remote 'command -v /opt/homebrew/bin/brew >/dev/null || /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)" </dev/null'
remote '/opt/homebrew/bin/brew list node >/dev/null 2>&1 || /opt/homebrew/bin/brew install node'
remote '/opt/homebrew/bin/brew list ripgrep >/dev/null 2>&1 || /opt/homebrew/bin/brew install ripgrep'
remote 'command -v ~/.cargo/bin/cargo >/dev/null || curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path'
# The fixture is installed by all three, so all three have to be there.
remote 'PATH=/opt/homebrew/bin:$PATH; command -v pnpm >/dev/null || npm install -g pnpm@10.28.0'
remote 'PATH=/opt/homebrew/bin:$PATH; command -v yarn >/dev/null || npm install -g yarn@1.22.22 --force'

step "Copying the repository"
# Not the build output, not the guest images, not the results: those are either
# reproducible there or specific to here.
rsync -a --delete \
	--exclude target/ --exclude guest/out/ --exclude guest/build/ --exclude guest/src/ \
	--exclude .git/ --exclude .logs/ --exclude .context/ --exclude vendor/ \
	--exclude 'benchmarks/results/*.csv' --exclude 'benchmarks/results/*.log' \
	"$ROOT/" "$HOST:$REMOTE_DIR/"

step "Guest artifacts"
# Copied rather than rebuilt, and the reasoning is not "reproducibility loses".
# The kernel takes twenty minutes to compile on a machine this size and under a
# minute to send over a LAN, and it has to match the tree that was just synced
# — which the local copy already does, because it was built from it. Rebuilding
# there would also need a container runtime on a machine that may not have one.
#
# `BUILD_GUEST=1` builds instead, for a host with no line of sight to a machine
# that has already done it.
if [ "${BUILD_GUEST:-0}" = 1 ]; then
	remote 'command -v ~/.orbstack/bin/docker >/dev/null || command -v /opt/homebrew/bin/colima >/dev/null' \
		|| { echo "no container runtime on $HOST: install OrbStack or Colima" >&2; exit 1; }
	remote 'if [ -x ~/.orbstack/bin/docker ]; then
			[ -S ~/.orbstack/run/docker.sock ] || { open -a OrbStack; until [ -S ~/.orbstack/run/docker.sock ]; do sleep 3; done; }
		else
			/opt/homebrew/bin/colima status >/dev/null 2>&1 || /opt/homebrew/bin/colima start
		fi'
	remote "cd $REMOTE_DIR && export PATH=\$HOME/.orbstack/bin:/opt/homebrew/bin:\$HOME/.cargo/bin:\$PATH &&
		bash guest/kernel/build.sh && bash guest/agent/build.sh &&
		bash guest/fstest/build.sh && bash guest/initramfs/build.sh &&
		bash guest/rootfs/build.sh"
else
	[ -f "$ROOT/guest/out/Image" ] || {
		echo "no guest artifacts to copy; build them here first, or set BUILD_GUEST=1" >&2
		exit 1
	}
	remote "mkdir -p $REMOTE_DIR/guest/out"
	rsync -a "$ROOT/guest/out/" "$HOST:$REMOTE_DIR/guest/out/"
fi

step "The network sidecar"
remote "cd $REMOTE_DIR && bash scripts/fetch-gvproxy.sh"

step "Building and signing"
remote "cd $REMOTE_DIR && export PATH=\$HOME/.cargo/bin:/opt/homebrew/bin:\$HOME/.orbstack/bin:\$PATH &&
	cargo build --release --example boot -p lighter-vmm &&
	./scripts/sign.sh target/release/examples/boot"

step "Ready"
cat <<EOF
Run a suite there with, for a machine this size:

  ssh $HOST 'cd $REMOTE_DIR && PATH=/opt/homebrew/bin:\$HOME/.cargo/bin:\$PATH \\
    BENCH_MEMORY_MIB=2048 BENCH_CPUS=4 benchmarks/run.sh --target native --reps 3'

Numbers from there are not comparable with numbers from here — different
silicon, different thermal envelope — but they are comparable with each other,
which is the whole point of measuring on a machine nobody is using.
EOF
