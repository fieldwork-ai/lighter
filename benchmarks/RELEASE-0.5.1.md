# 0.5.1 performance measurements

Runtime `b78dfeb` uses hybrid RAM preparation. Each host contributes one complete suite with three repetitions per timed case. Memory and idle-power rows are single observation windows. M5 uses 8 vCPUs / 16 GiB guest RAM; M1 uses 8 vCPUs / 4 GiB. Both use a 128 GiB sparse disk and pinned tools and images.

The M5 primary is the fresh suite after renewed quiet clearance. Earlier attempts stopped at guest preflight when the daily VM restarted; their completed share stage and the later shared-host copy diagnostics are retained separately and are not selected. All primary stages pass their quiet preflight, competing-VM guard and artifact checks.

Quiet preflight requires six observations at most 5% aggregate host CPU, ten seconds apart. Host-load and fseventsd observations are retained throughout. The original measured repetitions, including any cold first observations, are never discarded. CV below is sample standard deviation divided by the mean of repetitions within this suite; it is not a bound on variation between independent runs.

The 0.5.0 columns are historical primary records, not an alternating old/new experiment. A difference alone does not establish a runtime effect. Negative change means a lower measurement: better for time/memory/CPU, but worse for throughput or connection rate. The focused M1 follow-ups did not reproduce a consistent package-install slowdown.

[Design and focused investigations](../docs/demand-memory-2026-09-08.md) · [Repeatability](REPEATABILITY.md) · [Superseded quarter-RAM measurements](RELEASE-0.5.1-QUARTER.md)

## M5 — share

| Case | Unit | 0.5.0 median | 0.5.1 observations | 0.5.1 median | Change | Within-suite CV |
|---|---|---:|---|---:|---:|---:|
| npm-install | ms | 6302 | 6347, 6364, 6718 | 6364 | +1.0% | 3.23% |
| pnpm-install | ms | 3857 | 5087, 3975, 4008 | 4008 | +3.9% | 14.52% |
| yarn-install | ms | 5124 | 5543, 5192, 5051 | 5192 | +1.3% | 4.81% |
| ripgrep | ms | 91 | 1838, 88, 82 | 88 | -3.3% | 151.21% |
| find-walk | ms | 95 | 98, 92, 90 | 92 | -3.2% | 4.46% |
| copy-tree | ms | 3587 | 3626, 3804, 4025 | 3804 | +6.0% | 5.23% |
| rm-rf | ms | 2639 | 2642, 3662, 3224 | 3224 | +22.2% | 16.11% |
| cpu-sha256 | ms | 3011 | 3023, 3018, 2995 | 3018 | +0.2% | 0.50% |
| container-start | ms | 152 | 138, 151, 137 | 138 | -9.2% | 5.50% |
| watch-latency | ms | 6 | 46, 1, 2 | 2 | -66.7% | 157.33% |
| memory-peak | MiB | 4386 | 3841 | 3841 | -12.4% | — |
| memory-after-15s | MiB | 936 | 702 | 702 | -25.0% | — |
| memory-after-60s | MiB | 936 | 726 | 726 | -22.4% | — |
| net-tcp-egress | Mbit/s | 92475 | 90994, 94945, 88832 | 90994 | -1.6% | 3.38% |
| net-tcp-egress-r | Mbit/s | 84924 | 83692, 86990, 83316 | 83692 | -1.5% | 2.39% |
| net-tcp-port | Mbit/s | 83763 | 87001, 83599, 87104 | 87001 | +3.9% | 2.32% |
| net-tcp-port-r | Mbit/s | 88759 | 91251, 92046, 94591 | 92046 | +3.7% | 1.88% |
| net-udp | Mbit/s | 4993 | 4915, 5068, 5085 | 5068 | +1.5% | 1.86% |
| net-connect-rate | connections/s | 16828 | 17205, 17106, 16184 | 17106 | +1.7% | 3.35% |
| net-http-p99 | us | 154 | 280, 213, 174 | 213 | +38.3% | 24.11% |
| net-http-latency | us | 60 | 70, 64, 62 | 64 | +6.7% | 6.37% |
| net-dns | us | 37 | 40, 40, 40 | 40 | +8.1% | 0.00% |
| power-cpu-ms-per-s | CPU ms/s | 3 | 3 | 3 | +0.0% | — |
| power-wakeups-per-s | wakeups/s | 61 | 56 | 56 | -8.2% | — |
| power-pkg-idle-wakeups-per-s | wakeups/s | 1 | 1 | 1 | +0.0% | — |
| boot-docker | ms | 1649 | 521, 517, 494 | 517 | -68.6% | 2.85% |
| boot-first-container | ms | 1799 | 664, 672, 653 | 664 | -63.1% | 1.44% |
| memory-idle | MiB | 365 | 372 | 372 | +1.9% | — |

## M5 — guest

| Case | Unit | 0.5.0 median | 0.5.1 observations | 0.5.1 median | Change | Within-suite CV |
|---|---|---:|---|---:|---:|---:|
| npm-install | ms | 4605 | 4550, 4310, 4487 | 4487 | -2.6% | 2.80% |
| pnpm-install | ms | 1216 | 1220, 1143, 1243 | 1220 | +0.3% | 4.36% |
| yarn-install | ms | 4152 | 4101, 3986, 4310 | 4101 | -1.2% | 3.97% |
| ripgrep | ms | 79 | 88, 85, 83 | 85 | +7.6% | 2.95% |
| find-walk | ms | 97 | 92, 95, 97 | 95 | -2.1% | 2.66% |
| copy-tree | ms | 937 | 898, 909, 873 | 898 | -4.2% | 2.07% |
| rm-rf | ms | 387 | 399, 396, 386 | 396 | +2.3% | 1.73% |

## M5 — amd64

| Case | Unit | 0.5.0 median | 0.5.1 observations | 0.5.1 median | Change | Within-suite CV |
|---|---|---:|---|---:|---:|---:|
| npm-install | ms | 9237 | 9233, 9144, 9065 | 9144 | -1.0% | 0.92% |
| pnpm-install | ms | 2735 | 2822, 2747, 2733 | 2747 | +0.4% | 1.73% |
| cpu-sha256 | ms | 4223 | 4154, 4177, 4163 | 4163 | -1.4% | 0.28% |
| container-start | ms | 155 | 137, 156, 165 | 156 | +0.6% | 9.36% |

[Complete M5 records](../docs/records/0.5.1/hybrid/m5-full/)

## M1 — share

| Case | Unit | 0.5.0 median | 0.5.1 observations | 0.5.1 median | Change | Within-suite CV |
|---|---|---:|---|---:|---:|---:|
| npm-install | ms | 11235 | 13294, 12849, 12115 | 12849 | +14.4% | 4.67% |
| pnpm-install | ms | 6679 | 8658, 6690, 7561 | 7561 | +13.2% | 12.91% |
| yarn-install | ms | 10933 | 14413, 10969, 10490 | 10969 | +0.3% | 17.90% |
| ripgrep | ms | 192 | 5925, 257, 129 | 257 | +33.9% | 157.34% |
| find-walk | ms | 134 | 132, 121, 121 | 121 | -9.7% | 5.09% |
| copy-tree | ms | 7888 | 7094, 8885, 7593 | 7593 | -3.7% | 11.76% |
| rm-rf | ms | 2669 | 2697, 2827, 2838 | 2827 | +5.9% | 2.81% |
| cpu-sha256 | ms | 6296 | 6291, 6273, 6600 | 6291 | -0.1% | 2.88% |
| container-start | ms | 204 | 233, 188, 185 | 188 | -7.8% | 13.31% |
| watch-latency | ms | 3 | 462, 3, 2 | 3 | +0.0% | 170.42% |
| memory-peak | MiB | 4123 | 4109 | 4109 | -0.3% | — |
| memory-after-15s | MiB | 813 | 818 | 818 | +0.6% | — |
| memory-after-60s | MiB | 807 | 817 | 817 | +1.2% | — |
| net-tcp-egress | Mbit/s | 55819 | 55046, 56692, 55911 | 55911 | +0.2% | 1.47% |
| net-tcp-egress-r | Mbit/s | 49273 | 49210, 49280, 50340 | 49280 | +0.0% | 1.28% |
| net-tcp-port | Mbit/s | 48211 | 48682, 49105, 48756 | 48756 | +1.1% | 0.46% |
| net-tcp-port-r | Mbit/s | 54030 | 54727, 53668, 54963 | 54727 | +1.3% | 1.27% |
| net-udp | Mbit/s | 4829 | 4872, 4987, 5032 | 4987 | +3.3% | 1.66% |
| net-connect-rate | connections/s | 10607 | 11024, 14942, 26147 | 14942 | +40.9% | 45.18% |
| net-http-p99 | us | 249 | 333, 244, 216 | 244 | -2.0% | 23.11% |
| net-http-latency | us | 132 | 156, 130, 126 | 130 | -1.5% | 11.86% |
| net-dns | us | 135 | 130, 202, 128 | 130 | -3.7% | 27.49% |
| power-cpu-ms-per-s | CPU ms/s | 6 | 5 | 5 | -16.7% | — |
| power-wakeups-per-s | wakeups/s | 51 | 53 | 53 | +3.9% | — |
| power-pkg-idle-wakeups-per-s | wakeups/s | 1 | 0 | 0 | -100.0% | — |
| boot-docker | ms | 846 | 782, 720, 700 | 720 | -14.9% | 5.82% |
| boot-first-container | ms | 1030 | 960, 905, 876 | 905 | -12.1% | 4.67% |
| memory-idle | MiB | 248 | 241 | 241 | -2.8% | — |

## M1 — guest

| Case | Unit | 0.5.0 median | 0.5.1 observations | 0.5.1 median | Change | Within-suite CV |
|---|---|---:|---|---:|---:|---:|
| npm-install | ms | 7584 | 7694, 7566, 7424 | 7566 | -0.2% | 1.79% |
| pnpm-install | ms | 1760 | 1990, 1669, 1680 | 1680 | -4.5% | 10.24% |
| yarn-install | ms | 7828 | 8219, 7816, 7739 | 7816 | -0.2% | 3.25% |
| ripgrep | ms | 131 | 589, 146, 122 | 146 | +11.5% | 92.05% |
| find-walk | ms | 121 | 125, 122, 120 | 122 | +0.8% | 2.06% |
| copy-tree | ms | 4054 | 4328, 3905, 2535 | 3905 | -3.7% | 26.11% |
| rm-rf | ms | 603 | 564, 613, 621 | 613 | +1.7% | 5.15% |

## M1 — amd64

| Case | Unit | 0.5.0 median | 0.5.1 observations | 0.5.1 median | Change | Within-suite CV |
|---|---|---:|---|---:|---:|---:|
| npm-install | ms | 15004 | 15820, 15209, 14882 | 15209 | +1.4% | 3.11% |
| pnpm-install | ms | 3897 | 4146, 3748, 3760 | 3760 | -3.5% | 5.83% |
| cpu-sha256 | ms | 7182 | 7185, 7166, 7182 | 7182 | +0.0% | 0.14% |
| container-start | ms | 187 | 256, 207, 237 | 237 | +26.7% | 10.59% |

[Complete M1 records](../docs/records/0.5.1/hybrid/m1-full/)

## Reproduce the summaries

```sh
python3 scripts/records/summarize-hybrid-release.py docs/records/0.5.1/hybrid/m5-full --out /tmp/lighter-m5-summary.json
python3 scripts/records/summarize-hybrid-release.py docs/records/0.5.1/hybrid/m1-full --out /tmp/lighter-m1-summary.json
```

The validator checks each CSV hash, source identity, completed guard, quiet window, expected cases and repetition counts before calculating the summary. These full-suite timings use the Node-based harness; do not pool them with the separate Python-based startup experiments or attribute pure-demand prototype timings to the hybrid release.
