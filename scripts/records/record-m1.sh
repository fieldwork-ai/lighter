#!/usr/bin/env bash
# Copied from .logs/m1-record-vmem.sh on 2026-09-06: run it from the repo root on the M5; it ships the remote script over ssh and starts the record on the M1 (results back by rsync into benchmarks/results/machines/m1/ by hand).
# The M1's lighter records on the virtio-mem tree: fetched from GitHub and built there, nothing rsynced.
set -u
ssh -o ConnectTimeout=20 -o BatchMode=yes admin@100.125.161.101 'cat > ~/remote-record.sh <<'"'"'EOS'"'"'
#!/bin/bash
set -u; cd ~/lighter; mkdir -p .logs
export PATH=$HOME/.orbstack/bin:/opt/homebrew/bin:$HOME/.cargo/bin:$PATH
export BENCH_MEMORY_MIB=4096 BENCH_CPUS=8
# The results files are tracked, and the last record rewrote them: copied aside, then restored, or the fast-forward refuses and the record runs on the old tree (it did, once).
mkdir -p ~/records-aside && cp benchmarks/results/lighter.csv ~/records-aside/lighter-$(date +%H%M).csv 2>/dev/null; cp benchmarks/results/lighter-guest.csv ~/records-aside/lighter-guest-$(date +%H%M).csv 2>/dev/null
git checkout -q -- benchmarks/results/lighter.csv benchmarks/results/lighter-guest.csv benchmarks/results/lighter-amd64.csv benchmarks/results/lighter.tree benchmarks/results/lighter-guest.tree benchmarks/results/lighter-amd64.tree benchmarks/RESULTS.md 2>/dev/null; rm -f benchmarks/results/lighter-amd64.tree
git fetch -q origin dev && git merge -q --ff-only origin/dev || { echo "fast-forward failed"; git status --short | head -5; exit 1; }
git log --oneline -1
cargo build --release --example lighter-bench -p lighter-vmm 2>&1 | grep -E "^error|Finished"; cargo build --release 2>&1 | grep -E "^error|Finished"
./scripts/sign.sh target/release/lighter >/dev/null; ./scripts/sign.sh target/release/examples/lighter-bench >/dev/null
# The kernel, the rootfs and the agent come from the M5, byte-identical to what it recorded with, rather than being rebuilt here (twenty minutes of an 8 GB machine with colima beside it per change); the rebuild below stays as the fallback for artifacts the M5 does not have.
[ -f ~/m5-artifacts/Image ] && cp ~/m5-artifacts/Image guest/out/Image && cp ~/m5-artifacts/rootfs.ext4 guest/out/rootfs.ext4 && cp ~/m5-artifacts/lighter-agent guest/out/lighter-agent && touch guest/out/Image guest/out/rootfs.ext4 guest/out/lighter-agent && echo "artifacts: from the M5 ($(md5 -q guest/out/Image | cut -c1-8))"
if [ -n "$(find guest/kernel -newer guest/out/Image -print -quit)" ]; then
	echo "kernel: rebuilding (colima is the build daemon)"; colima start >/dev/null 2>&1; docker context use colima >/dev/null 2>&1; docker rm -f lighter-kbuild-250 >/dev/null 2>&1
	LIGHTER_KERNEL_ONLY=250 ./guest/kernel/build.sh > .logs/kernel.log 2>&1; echo "kernel rc=$?"; tail -1 .logs/kernel.log
	[ -z "$(find guest/kernel -newer guest/out/Image -print -quit)" ] || { echo "kernel build failed; stopping"; exit 1; }
fi
if [ -n "$(find guest/agent/src guest/agent/Cargo.toml guest/rootfs/init guest/rootfs/Dockerfile -newer guest/out/rootfs.ext4 -print -quit)" ]; then
	# The agent is a Docker build: colima must be up for it (it was only up when the kernel needed rebuilding, and every rootfs build after that failed silently).
	colima start >/dev/null 2>&1; docker context use colima >/dev/null 2>&1
	./guest/rootfs/build.sh > .logs/rootfs.log 2>&1; echo "rootfs rc=$?"; tail -1 .logs/rootfs.log
	[ -z "$(find guest/agent/src guest/agent/Cargo.toml -newer guest/out/lighter-agent -print -quit)" ] || { echo "agent is stale; stopping"; exit 1; }
	[ -z "$(find guest/rootfs/init guest/rootfs/Dockerfile -newer guest/out/rootfs.ext4 -print -quit)" ] || { echo "rootfs is stale; stopping"; exit 1; }
fi
ALL="npm-install pnpm-install yarn-install ripgrep find-walk copy-tree rm-rf cpu-sha256 container-start watch-latency memory net-tcp-egress net-tcp-egress-r net-tcp-port net-tcp-port-r net-udp net-connect-rate net-http-latency net-dns power-idle boot"
GUEST="npm-install pnpm-install yarn-install ripgrep find-walk copy-tree rm-rf"
stage() { echo "==> STAGE $1 $(date +%H:%M)"; }
settle() { colima stop >/dev/null 2>&1; osascript -e "quit app \"Docker Desktop\"" >/dev/null 2>&1; osascript -e "quit app \"Docker\"" >/dev/null 2>&1; sleep 8; pkill -f "Docker Desktop" 2>/dev/null; pkill -f "docker-agent serve" 2>/dev/null; pkill -f "com.docker" 2>/dev/null; sleep 5; echo "free pages: $(vm_stat | awk "/Pages free/ {print \$3}")"; }
# A record straight after a build ran on a host still indexing and journalling the files the build wrote: two M1 records on 2026-09-06 read share ripgrep at 6 s on every rep and copy-tree doubled, and the same sequence an hour later read 140 ms and 4.5 s. Wait until the indexers are quiet (up to ten minutes) before the first stage.
indexers_busy() { ps -Ao pcpu,comm | awk "/mds_stores|mdworker|fseventsd|photolibraryd|mediaanalysisd/ && \$1 > 1 {n++} END {print n+0}"; }
quiet() { local i ok=0; for i in $(seq 1 90); do if [ "$(indexers_busy)" -eq 0 ]; then ok=$((ok+1)); [ "$ok" -ge 6 ] && { echo "host quiet for 60s after $((i*10))s"; return; }; else ok=0; fi; sleep 10; done; echo "host still busy after 15 min; recording anyway"; ps -Ao pcpu,comm -r | head -4; }
settle; orb stop >/dev/null 2>&1; sleep 3; quiet
stage lighter-share; scripts/capped.sh 3600 ./benchmarks/run.sh --target lighter --reps 3 --cases "$ALL" --allow-noisy > .logs/rec-lighter-share.log 2>&1; echo "lighter-share=$?"
stage lighter-guest; scripts/capped.sh 3000 ./benchmarks/run.sh --target lighter --reps 3 --where guest --cases "$GUEST" --allow-noisy > .logs/rec-lighter-guest.log 2>&1; echo "lighter-guest=$?"
echo "M1-RECORD-DONE $(date +%H:%M)"
EOS
chmod +x ~/remote-record.sh; nohup ~/remote-record.sh > ~/record.log 2>&1 &' && echo "M1-RECORD-STARTED"
