# 0.5.1 performance measurements

Three complete M1 suites use the same frozen runtime, eight vCPUs, 4 GiB guest RAM, a 128 GiB sparse disk and pinned package tools and images. The first valid suite remains the primary record. Each timing is the median of three successful repetitions; memory and idle-power rows are sampling windows. Between-run CV is sample standard deviation divided by the arithmetic mean of these three values.

Each stage requires six aggregate CPU observations at most 5%, ten seconds apart, and rejects unexpected VMs throughout. CPU and daemon observations remain available for assessing interference during workloads. No daemon was reset. The ABBA storage follow-up ran between full suites one and two. Warm-up/setup exit statuses were not retained by the inherited protocol; the archived measured-case diagnostics cannot validate those statuses retrospectively.

The M5 remained shared. No full M5 suite was run for 0.5.1; the README's complete comparison remains the 0.5.0 M5 record.

[Raw records and selection](../docs/records/0.5.1/benchmarks/) · [Repeatability](REPEATABILITY.md)

## M1 — share

| Metric | Unit | Primary | Run 2 | Run 3 | Between-run CV |
|---|---|---:|---:|---:|---:|
| npm-install | ms | 12413 | 12833 | 11604 | 5.09% |
| pnpm-install | ms | 7255 | 7363 | 6658 | 5.35% |
| yarn-install | ms | 11961 | 11305 | 11029 | 4.19% |
| ripgrep | ms | 501 | 256 | 311 | 36.11% |
| find-walk | ms | 125 | 148 | 126 | 9.77% |
| copy-tree | ms | 8683 | 8184 | 7916 | 4.71% |
| rm-rf | ms | 2840 | 2848 | 2875 | 0.64% |
| cpu-sha256 | ms | 6299 | 6299 | 6287 | 0.11% |
| container-start | ms | 207 | 195 | 203 | 3.03% |
| watch-latency | ms | 4 | 2 | 14 | 96.44% |
| memory-peak | MiB | 3426 | 4177 | 3187 | 14.36% |
| memory-after-15s | MiB | 866 | 784 | 726 | 8.88% |
| memory-after-60s | MiB | 858 | 786 | 731 | 8.04% |
| net-tcp-egress | Mbit/s | 56481 | 56419 | 56271 | 0.19% |
| net-tcp-egress-r | Mbit/s | 49936 | 49225 | 50005 | 0.87% |
| net-tcp-port | Mbit/s | 48273 | 48193 | 47918 | 0.39% |
| net-tcp-port-r | Mbit/s | 54242 | 54106 | 54864 | 0.74% |
| net-udp | Mbit/s | 4784 | 4794 | 4869 | 0.96% |
| net-connect-rate | connections/s | 18575 | 10545 | 16908 | 27.62% |
| net-http-p99 | µs | 233 | 247 | 262 | 5.86% |
| net-http-latency | µs | 131 | 131 | 133 | 0.88% |
| net-dns | µs | 129 | 125 | 127 | 1.57% |
| power-cpu-ms-per-s | CPU ms/s | 6 | 6 | 5 | 10.19% |
| power-wakeups-per-s | wakeups/s | 53 | 50 | 53 | 3.33% |
| power-pkg-idle-wakeups-per-s | wakeups/s | 1 | 1 | 1 | 0.00% |
| boot-docker | ms | 634 | 643 | 652 | 1.40% |
| boot-first-container | ms | 821 | 859 | 834 | 2.30% |
| memory-idle | MiB | 251 | 258 | 249 | 1.87% |

## M1 — guest

| Metric | Unit | Primary | Run 2 | Run 3 | Between-run CV |
|---|---|---:|---:|---:|---:|
| npm-install | ms | 8087 | 7630 | 7562 | 3.68% |
| pnpm-install | ms | 1727 | 1728 | 1656 | 2.42% |
| yarn-install | ms | 7873 | 7728 | 7921 | 1.28% |
| ripgrep | ms | 133 | 131 | 142 | 4.33% |
| find-walk | ms | 121 | 120 | 121 | 0.48% |
| copy-tree | ms | 3586 | 3869 | 4141 | 7.18% |
| rm-rf | ms | 604 | 594 | 591 | 1.14% |

## M1 — amd64

| Metric | Unit | Primary | Run 2 | Run 3 | Between-run CV |
|---|---|---:|---:|---:|---:|
| npm-install | ms | 14895 | 15023 | 14757 | 0.89% |
| pnpm-install | ms | 3891 | 3849 | 3852 | 0.61% |
| cpu-sha256 | ms | 7171 | 7224 | 7178 | 0.40% |
| container-start | ms | 200 | 181 | 183 | 5.55% |

## Matched storage comparison

The first suite appeared slower than the historical 0.5.0 record on several share workloads, prompting this follow-up. Both VMMs were rebuilt with the same compiler, using each version's guest payload. Order is 0.5.0 / 0.5.1 / 0.5.1 / 0.5.0, with three repetitions per arm. Change is the ratio of geometric means of arm medians minus one; positive means longer elapsed time.

| Case | Ordered arm medians (ms) | 0.5.1 time change |
|---|---|---:|
| npm-install | 12855, 12173, 12879, 12333 | -0.56% |
| pnpm-install | 7201, 6850, 6691, 7382 | -7.14% |
| yarn-install | 11344, 11711, 11215, 11466 | +0.49% |
| ripgrep | 382, 266, 261, 420 | -34.22% |
| find-walk | 155, 165, 130, 129 | +3.57% |
| copy-tree | 7712, 7834, 7661, 8550 | -4.60% |
| rm-rf | 2792, 2905, 2889, 2917 | +1.51% |

The broad historical install/copy slowdown did not reproduce. The small increases in find and removal remain in the record. Two arms per version cannot establish a precise causal effect, and measured variation is not an automatic threshold for declaring a difference noise.

## Startup measurement protocols

The full suite starts the checkout CLI with guest/out and a private development app, using Node wall-clock timestamps. The signed comparison starts the shipped Developer ID app and uses Python monotonic timestamps. Their absolute times are separate records; use the signed archive comparison below for the like-for-like release startup claim.

## Signed archive startup — image-only

The published 0.5.0 and final 0.5.1 archives use identical 8-vCPU, 4-GiB, 128-GiB profiles and the same Alpine image. Each arm has one untimed preparation round and five retained cold starts. An outer guard enforces the quiet-host and VM checks; the inner recorder does not itself enforce quietness.

| Arm | Version | Docker median | Within-arm CV | First-container median | Within-arm CV |
|---|---|---:|---:|---:|---:|
| 1 | 0.5.0 | 711.3 ms | 2.85% | 895.3 ms | 2.40% |
| 2 | 0.5.1 | 534.9 ms | 1.90% | 710.8 ms | 1.97% |
| 3 | 0.5.1 | 537.6 ms | 3.87% | 719.0 ms | 3.25% |
| 4 | 0.5.0 | 721.7 ms | 0.66% | 906.6 ms | 0.44% |

Using geometric means of the two arm medians per version:

- Docker readiness: 716.5 → 536.3 ms (-25.15% elapsed time).
- First-container completion: 900.9 → 714.9 ms (-20.65% elapsed time).

These image-only starts do not take the saved-container memory gate.

## Signed archive startup — saved

The published 0.5.0 and final 0.5.1 archives use identical 8-vCPU, 4-GiB, 128-GiB profiles and the same Alpine image. Each arm has one untimed preparation round and five retained cold starts. An outer guard enforces the quiet-host and VM checks; the inner recorder does not itself enforce quietness.

| Arm | Version | Docker median | Within-arm CV | First-container median | Within-arm CV |
|---|---|---:|---:|---:|---:|
| 1 | 0.5.0 | 744.9 ms | 2.54% | 925.4 ms | 2.09% |
| 2 | 0.5.1 | 581.3 ms | 6.55% | 757.2 ms | 5.55% |
| 3 | 0.5.1 | 581.2 ms | 4.58% | 761.4 ms | 3.02% |
| 4 | 0.5.0 | 764.1 ms | 6.45% | 944.1 ms | 5.51% |

Using geometric means of the two arm medians per version:

- Docker readiness: 754.4 → 581.3 ms (-22.95% elapsed time).
- First-container completion: 934.7 → 759.3 ms (-18.76% elapsed time).

A saved, stopped Alpine container triggers the pre-Docker memory gate in this profile. It isolates that gate's cost; it does not measure restoring an application or kind cluster.

## Filesystem-daemon observation

Across the three full suites, storage ABBA and ten-minute post-suite observation, 1349 observations tracked PID(s) [81590]. Sampled footprint was initially 13 MiB, peaked at 16 MiB and ended at 14 MiB. Post-suite CPU had median 0% and maximum 0.3% of one core. There were 0 collection errors and 0 invalid CPU observations.

Sampling every five seconds can miss shorter peaks, and memory inherits top's display rounding. The ten-minute window precedes the later signed-archive tests. These observations do not establish a fix for an earlier incident whose trigger was not reproduced.

## Interpretation and selection

Full-suite attempt 1 stopped before measurements because the checkout lacked the published v0.5.0 tag. Attempt 2 is the first valid complete suite and remains primary. No slow measured repetition was discarded. The paired storage follow-up was selected after inspecting the historical difference; both the original observation and follow-up remain available.

Docker probes include command execution and a 50 ms interval after failures. First-container completion includes any remaining memory preparation wait. These results describe the tested host, configuration and workload; they are not universal startup times or variance bounds. Configured guest RAM is distinct from macOS process footprint, which includes host overhead and compressed-memory charges.
