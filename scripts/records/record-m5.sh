#!/bin/bash
# The M5's lighter records on the virtio-mem tree (Nick's daily driver stays up; --allow-noisy).
set -u; cd ~/git/lighter
ALL="npm-install pnpm-install yarn-install ripgrep find-walk copy-tree rm-rf cpu-sha256 container-start watch-latency memory net-tcp-egress net-tcp-egress-r net-tcp-port net-tcp-port-r net-udp net-connect-rate net-http-latency net-dns power-idle boot"
GUEST="npm-install pnpm-install yarn-install ripgrep find-walk copy-tree rm-rf"
echo "==> STAGE lighter-share $(date +%H:%M)"; scripts/capped.sh 3600 ./benchmarks/run.sh --target lighter --reps 3 --cases "$ALL" --allow-noisy > .logs/rec-m5-lighter-share.log 2>&1; echo "lighter-share=$?"
echo "==> STAGE lighter-guest $(date +%H:%M)"; scripts/capped.sh 3000 ./benchmarks/run.sh --target lighter --reps 3 --where guest --cases "$GUEST" --allow-noisy > .logs/rec-m5-lighter-guest.log 2>&1; echo "lighter-guest=$?"
echo "M5-RECORD-DONE $(date +%H:%M)"
