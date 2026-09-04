#!/usr/bin/env bash
# Fast iteration on the guest agent: build it, drop it in the M1's bench share,
# and run one repetition of the storage cases plus the memory case there with
# the guest told to pick the development copy up (lighter.devagent=...).
# Usage: scripts/iter-agent.sh <label>   → results in the M1's benchmarks/results/<label>.csv
set -u; cd ~/git/lighter; label=${1:-iter}
./guest/agent/build.sh > .logs/agent-build.log 2>&1 || { echo "agent build failed"; tail -5 .logs/agent-build.log; exit 1; }
rsync -a guest/out/lighter-agent admin@192.168.50.21:.lighter-bench/lighter/.lighter-agent
rsync -a -S --exclude target --exclude .logs --exclude 'benchmarks/results/*' --exclude guest/out -e "ssh -o ConnectTimeout=20" ./ admin@192.168.50.21:lighter/ >/dev/null
ssh admin@192.168.50.21 "cd ~/lighter && export PATH=\$HOME/.orbstack/bin:/opt/homebrew/bin:\$HOME/.cargo/bin:\$PATH BENCH_MEMORY_MIB=4096 BENCH_CPUS=8 LIGHTER_CMDLINE_EXTRA='lighter.devagent=/mnt/bench/.lighter-agent' && echo '== M1 iter $label '\$(date +%H:%M) && scripts/capped.sh 900 ./benchmarks/run.sh --target lighter --reps 1 --cases 'npm-install pnpm-install yarn-install ripgrep find-walk copy-tree rm-rf memory' --label $label --allow-noisy 2>&1 | grep -E '==> lighter: (npm|pnpm|yarn|ripgrep|find|copy|rm-rf|memory)|devagent' | sed -E 's/ +\(cpu.*//'; sed 's/\x1b\[[0-9;]*m//g' benchmarks/results/lighter-boot.log | grep -E 'INIT devagent|puff' | head -3"
echo "record installs: $(ssh admin@192.168.50.21 "grep -E '^(npm|pnpm|yarn)-install' ~/lighter/benchmarks/results/lighter.csv | cut -d, -f3 | tr '\n' ' '")"
echo "record memory: 1263 3875 1029 1014"
