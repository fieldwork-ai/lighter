#!/usr/bin/env bash
# Fast iteration on the guest agent: build it, drop it in the M1's bench share,
# and run one repetition of the storage cases plus the memory case there with
# the guest told to pick the development copy up (`LIGHTER_BENCH_DEV_AGENT`
# in run.sh: a second share, and `lighter.devagent=` on the command line).
# Usage: scripts/iter-agent.sh <label>   → results in the M1's benchmarks/results/<label>.csv
set -u; cd "$(dirname "${BASH_SOURCE[0]}")/.."; label=${1:-iter}
# The bench host, user@address; the M1 on the LAN or over Tailscale.
M1="${LIGHTER_BENCH_HOST:?set LIGHTER_BENCH_HOST=user@host}"
# Builds go to OrbStack: the lighter context is the daily driver, and a
# kernel build there is sixteen gcc processes on its owner's machine.
export DOCKER_CONTEXT=orbstack
./guest/agent/build.sh > .logs/agent-build.log 2>&1 || { echo "agent build failed"; tail -5 .logs/agent-build.log; exit 1; }
ssh "$M1" 'mkdir -p ~/.lighter-bench/dev'; rsync -a guest/out/lighter-agent "$M1":.lighter-bench/dev/lighter-agent
rsync -a -S --exclude target --exclude .logs --exclude 'benchmarks/results/*' --exclude guest/out -e "ssh -o ConnectTimeout=20" ./ "$M1":lighter/ >/dev/null
ssh "$M1" "cd ~/lighter && export PATH=\$HOME/.orbstack/bin:/opt/homebrew/bin:\$HOME/.cargo/bin:\$PATH BENCH_MEMORY_MIB=4096 BENCH_CPUS=8 LIGHTER_BENCH_DEV_AGENT=\$HOME/.lighter-bench/dev/lighter-agent && echo '== M1 iter $label '\$(date +%H:%M) && scripts/capped.sh 900 ./benchmarks/run.sh --target lighter --reps ${REPS:-1} --cases 'npm-install pnpm-install yarn-install ripgrep find-walk copy-tree rm-rf memory' --label $label --allow-noisy 2>&1 | grep -E '==> lighter: (npm|pnpm|yarn|ripgrep|find|copy|rm-rf|memory)|devagent' | sed -E 's/ +\(cpu.*//'; sed 's/\x1b\[[0-9;]*m//g' benchmarks/results/lighter-boot.log | grep -E 'INIT devagent|puff' | head -3"
echo "record installs: $(ssh "$M1" "grep -E '^(npm|pnpm|yarn)-install' ~/lighter/benchmarks/results/lighter.csv | cut -d, -f3 | tr '\n' ' '")"
echo "record memory: 1263 3875 1029 1014"
