# 0.4.1 release measurements

The full records below were taken on runtime commit `38bfab4`, after all twelve hardware gates passed on both Macs. The M5 has 18 cores and 48 GiB RAM; the M1 has eight cores and 8 GiB RAM. Both run macOS 26.6.2 (25G83), with Rust 1.98.0 used to build the candidate host executables. Each workload stage uses a fresh VM, eight vCPUs and a fixed 128 GiB sparse disk. Timing cases use three repetitions after cache warming; memory and power rows each describe one workload or sampling window, not three independent runs. Workload RAM is 16 GiB on M5 and 4 GiB on M1. Cold-start measurements use the CLI defaults: 12 GiB/nine vCPUs on M5 and 2 GiB/four vCPUs on M1.

Other container runtimes and the M5 daily VM were stopped. A minute of low filesystem/indexer activity was required before each full record. Ordinary host background applications remained; M5 background activity was explicitly sampled. These checks reduce interference without establishing exclusive use of every CPU or a universal noise threshold.

Full records are retained as measured, including outliers. Percentages below compare published run medians across sessions and do not by themselves establish a causal speedup or regression. The matched comparisons further down use alternating old/new runs. The established same-build variation is specific to M1 share storage; it supplies no automatic threshold for other hosts, networking, memory, boot or amd64. See [repeatability](REPEATABILITY.md).

Memory rows are the macOS task footprint, including compressed-memory charges and host overhead. The 0.4.1 fix removes a duplicate charge; these percentage reductions are not equivalent reductions in distinct physical RAM. Independent reclamation and the real 24/12/8 GiB application-build tests are documented in the [memory investigation](../docs/memory-accounting-2026-09-06.md).

## Full records

### M5 — share

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 6213 | 6334 | +1.9% | lower |
| pnpm-install | ms | 4169 | 4025 | -3.5% | lower |
| yarn-install | ms | 5442 | 5328 | -2.1% | lower |
| ripgrep | ms | 85 | 82 | -3.5% | lower |
| find-walk | ms | 88 | 93 | +5.7% | lower |
| copy-tree | ms | 3751 | 3786 | +0.9% | lower |
| rm-rf | ms | 2508 | 2522 | +0.6% | lower |
| cpu-sha256 | ms | 3002 | 2995 | -0.2% | lower |
| container-start | ms | 140 | 167 | +19.3% | lower |
| watch-latency | ms | 2 | 3 | +50.0% | lower |
| memory-peak | MiB | 5423 | 4300 | -20.7% | lower |
| memory-after-15s | MiB | 1143 | 993 | -13.1% | lower |
| memory-after-60s | MiB | 1174 | 1028 | -12.4% | lower |
| net-tcp-egress | Mbit/s | 99299 | 92230 | -7.1% | higher |
| net-tcp-egress-r | Mbit/s | 93903 | 87032 | -7.3% | higher |
| net-tcp-port | Mbit/s | 91547 | 84071 | -8.2% | higher |
| net-tcp-port-r | Mbit/s | 81717 | 91606 | +12.1% | higher |
| net-udp | Mbit/s | 5136 | 5002 | -2.6% | higher |
| net-connect-rate | connections/s | 17026 | 17046 | +0.1% | higher |
| net-http-p99 | µs | 173 | 168 | -2.9% | lower |
| net-http-latency | µs | 57 | 64 | +12.3% | lower |
| net-dns | µs | 41 | 38 | -7.3% | lower |
| power-cpu-ms-per-s | ms CPU/s | 3 | 3 | +0.0% | lower |
| power-wakeups-per-s | wakeups/s | 57 | 58 | +1.8% | lower |
| power-pkg-idle-wakeups-per-s | wakeups/s | 2 | 0 | -100.0% | lower |
| boot-docker | ms | 364 | 1207 | +231.6% | lower |
| boot-first-container | ms | 507 | 1362 | +168.6% | lower |
| memory-idle | MiB | 359 | 304 | -15.3% | lower |

### M5 — guest

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 4532 | 4581 | +1.1% | lower |
| pnpm-install | ms | 1186 | 1234 | +4.0% | lower |
| yarn-install | ms | 4036 | 4214 | +4.4% | lower |
| ripgrep | ms | 80 | 84 | +5.0% | lower |
| find-walk | ms | 92 | 93 | +1.1% | lower |
| copy-tree | ms | 873 | 1078 | +23.5% | lower |
| rm-rf | ms | 376 | 404 | +7.4% | lower |

### M5 — amd64

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 8966 | 9253 | +3.2% | lower |
| pnpm-install | ms | 2896 | 2755 | -4.9% | lower |
| cpu-sha256 | ms | 4146 | 4188 | +1.0% | lower |
| container-start | ms | 149 | 170 | +14.1% | lower |

### M1 — share

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 11728 | 10896 | -7.1% | lower |
| pnpm-install | ms | 6628 | 5593 | -15.6% | lower |
| yarn-install | ms | 13414 | 10320 | -23.1% | lower |
| ripgrep | ms | 180 | 228 | +26.7% | lower |
| find-walk | ms | 100 | 120 | +20.0% | lower |
| copy-tree | ms | 5493 | 5118 | -6.8% | lower |
| rm-rf | ms | 2695 | 2555 | -5.2% | lower |
| cpu-sha256 | ms | 6281 | 6284 | +0.0% | lower |
| container-start | ms | 201 | 208 | +3.5% | lower |
| watch-latency | ms | 2 | 2 | +0.0% | lower |
| memory-peak | MiB | 4525 | 3006 | -33.6% | lower |
| memory-after-15s | MiB | 947 | 730 | -22.9% | lower |
| memory-after-60s | MiB | 944 | 722 | -23.5% | lower |
| net-tcp-egress | Mbit/s | 56108 | 56222 | +0.2% | higher |
| net-tcp-egress-r | Mbit/s | 46735 | 49488 | +5.9% | higher |
| net-tcp-port | Mbit/s | 45367 | 48805 | +7.6% | higher |
| net-tcp-port-r | Mbit/s | 54227 | 54913 | +1.3% | higher |
| net-udp | Mbit/s | 4990 | 4879 | -2.2% | higher |
| net-connect-rate | connections/s | 14731 | 11486 | -22.0% | higher |
| net-http-p99 | µs | 216 | 260 | +20.4% | lower |
| net-http-latency | µs | 127 | 132 | +3.9% | lower |
| net-dns | µs | 130 | 122 | -6.2% | lower |
| power-cpu-ms-per-s | ms CPU/s | 7 | 7 | +0.0% | lower |
| power-wakeups-per-s | wakeups/s | 58 | 60 | +3.4% | lower |
| power-pkg-idle-wakeups-per-s | wakeups/s | 1 | 2 | +100.0% | lower |
| boot-docker | ms | 466 | 604 | +29.6% | lower |
| boot-first-container | ms | 649 | 781 | +20.3% | lower |
| memory-idle | MiB | 279 | 234 | -16.1% | lower |

### M1 — guest

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 8177 | 7599 | -7.1% | lower |
| pnpm-install | ms | 1810 | 1674 | -7.5% | lower |
| yarn-install | ms | 7953 | 7700 | -3.2% | lower |
| ripgrep | ms | 203 | 131 | -35.5% | lower |
| find-walk | ms | 124 | 120 | -3.2% | lower |
| copy-tree | ms | 4433 | 3063 | -30.9% | lower |
| rm-rf | ms | 630 | 585 | -7.1% | lower |

### M1 — amd64

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 17090 | 14996 | -12.3% | lower |
| pnpm-install | ms | 4178 | 3787 | -9.4% | lower |
| cpu-sha256 | ms | 7197 | 7148 | -0.7% | lower |
| container-start | ms | 201 | 194 | -3.5% | lower |

## Matched follow-ups

The alternating runners set both the inherited host soft and hard descriptor limits to 10,240. This matches the M1 kernel ceiling and is lower than the M5 full-record ceiling. The M5 matched copy case uses the guest disk, not the host share whose descriptor cache scales with that limit. Both versions within each comparison use identical limits; these reduced sequences are not interchangeable with the full records. Final qualification raises only the soft limit for tests, preserving the normal host hard ceiling.

Each host runs 0.4.0, 0.4.1, 0.4.1, 0.4.0, with a fresh VM for each run and three repetitions per timing. The M1 repeats the complete seven-case share storage sequence, container start, connection rate, HTTP latency and cold boot. The M5 repeats guest-disk copy-tree, container start, all four TCP directions, connection rate, HTTP latency and cold boot without preceding timed installs. The copy case was added after the full record showed a larger guest-copy difference; both versions use the same sequence. Both versions within each comparison use the same harness and settings; the reduced sequence is not interchangeable with the full record.

Each cell below is a run median. The final column compares the median of the two run medians for each version. Two runs per version can check large observed changes but do not establish a small universal speedup or a confidence interval. The CLI used for the old boot runs is the signed 0.4.0 release; the old benchmark executable was built from its source. Both versions use their own guest artifacts. Source and artifact stamps accompany every raw CSV.

### M1 alternating comparison

| Metric | Unit | Old 1 | New 1 | New 2 | Old 2 | Numeric change |
|---|---|---:|---:|---:|---:|---:|
| npm-install | ms | 11549 | 10804 | 10750 | 11267 | -5.5% |
| pnpm-install | ms | 6203 | 5562 | 5629 | 5731 | -6.2% |
| yarn-install | ms | 14753 | 10107 | 10008 | 12018 | -24.9% |
| ripgrep | ms | 6086 | 162 | 403 | 593 | -91.5% |
| find-walk | ms | 101 | 122 | 123 | 110 | +16.1% |
| copy-tree | ms | 5562 | 5331 | 5291 | 9953 | -31.5% |
| rm-rf | ms | 2898 | 2540 | 2572 | 2561 | -6.4% |
| container-start | ms | 191 | 200 | 190 | 201 | -0.5% |
| net-connect-rate | connections/s | 14396 | 7666 | 9127 | 13428 | -39.6% |
| net-http-p99 | µs | 335 | 276 | 534 | 242 | +40.4% |
| net-http-latency | µs | 128 | 134 | 210 | 128 | +34.4% |
| boot-docker | ms | 474 | 642 | 635 | 477 | +34.3% |
| boot-first-container | ms | 663 | 817 | 804 | 656 | +22.9% |
| memory-idle | MiB | 283 | 257 | 237 | 296 | -14.7% |

### M5 alternating comparison

| Metric | Unit | Old 1 | New 1 | New 2 | Old 2 | Numeric change |
|---|---|---:|---:|---:|---:|---:|
| copy-tree | ms | 891 | 897 | 879 | 980 | -5.1% |
| container-start | ms | 202 | 205 | 161 | 160 | +1.1% |
| net-tcp-egress | Mbit/s | 96070 | 100704 | 95557 | 96825 | +1.7% |
| net-tcp-egress-r | Mbit/s | 90253 | 85122 | 90480 | 89244 | -2.2% |
| net-tcp-port | Mbit/s | 87932 | 83000 | 88900 | 86111 | -1.2% |
| net-tcp-port-r | Mbit/s | 89731 | 92485 | 92650 | 87601 | +4.4% |
| net-connect-rate | connections/s | 16935 | 16870 | 17135 | 16883 | +0.6% |
| net-http-p99 | µs | 168 | 178 | 160 | 162 | +2.4% |
| net-http-latency | µs | 58 | 62 | 62 | 58 | +6.9% |
| boot-docker | ms | 406 | 1192 | 1257 | 381 | +211.2% |
| boot-first-container | ms | 572 | 1345 | 1435 | 548 | +148.2% |
| memory-idle | MiB | 379 | 313 | 308 | 365 | -16.5% |

## M1 warmed HTTP follow-up

One candidate run in the storage/churn sequence produced a 210 µs median HTTP result. A separate old/new/new/old comparison used fresh VMs, omitted the preceding storage and connection-churn cases, sent 10,000 untimed HTTP requests, waited two seconds, and then measured five batches of 2,000 requests over kept-alive connections. Both versions used the same harness variant. These results characterize warmed HTTP without prior churn; they do not replace the original sequence or isolate which preceding activity caused its outlier.

| Metric | Unit | Old 1 | New 1 | New 2 | Old 2 | Numeric change |
|---|---|---:|---:|---:|---:|---:|
| net-http-latency | µs | 122 | 126 | 125 | 123 | +2.4% |
| net-http-p99 | µs | 184 | 225 | 214 | 271 | -3.5% |

Warmed median HTTP changed from 122.5 to 125.5 µs (+3 µs, +2.4%). The large median difference from the original churn sequence was not reproduced here. Tail results varied, including the old version; neither this small median shift nor the p99 difference is a universal performance claim.

## Interpretation

The accounting correction is established by coherent host/guest access and independent physical-page tests, not by smaller numbers in this table. The full records show footprint-peak reductions of 20.7% on M5 and 33.6% on M1 relative to the published 0.4.0 records; those are reductions in the macOS charge, not equivalent physical-RAM savings.

Cold start has a clear cost. In the alternating comparisons, Docker readiness increased by 831 ms on M5 (393.5 to 1224.5 ms, +211.2%) and 163 ms on M1 (475.5 to 638.5 ms, +34.3%). First-container readiness increased by 830 and 151 ms respectively. Creating and initializing the owned mappings adds work that scales with the RAM ceiling. The reservation-only probes below quantify part of that work; the version comparisons include all release changes and do not isolate every contributor.

M1 metadata walks were slower in both candidate runs: 122/123 ms against 101/110 ms, a 17 ms (+16.1%) difference between the paired medians. Client connection-setup rate was also lower in the matched sequence: 7,666/9,127 against 14,396/13,428 connections/s (-39.6% by the paired medians). Individual setup-rate samples varied substantially. This metric counts client TCP handshakes followed immediately by close; it does not measure completed application requests. The capacity regression separately checks successful HTTP after bursts and resource drainage. The warmed HTTP follow-up above shows a 3 µs median increase (+2.4%), rather than the large difference in one post-storage/churn run.

M1 npm and yarn installation medians improved in both candidate runs: the paired changes are -5.5% and -24.9%. These describe this sequence and machine. The pnpm and removal differences are smaller, and five earlier same-build runs do not establish a universal significance threshold for these new version comparisons. The apparent -91.5% search and -31.5% copy improvements are dominated by unstable old-run results: old search medians were 6086/593 ms and old copy medians 5562/9953 ms. Keep the arithmetic and raw samples, but do not advertise those percentages as stable speedups. The long first-search pattern also occurs in the published 0.4.0 record (6284/180/136 ms).

The full M5 guest-copy record was 23.5% slower than the historical 0.4.0 record. The alternating check did not reproduce that slowdown: old medians were 891/980 ms and new medians 897/879 ms (-5.1% by the paired medians). M5 matched TCP directions changed between -2.2% and +4.4%, connection rate by +0.6%, and container start by +1.1%; these small differences are not universal speedup or regression claims. Median HTTP rose from 58 to 62 µs (+4 µs, +6.9%) in both candidate runs.

The M5's first fifteen-minute settling attempt ended without taking any measurements. A second attempt passed the same quiet check before the final record; no criterion was weakened and no measured run was discarded. Ordinary background applications remained, and host activity was sampled. M1's wallpaper process was paused during measurement. Neither setup proves exclusive use of every CPU. Power and memory rows are single measurement windows, and changes such as a rounded package-wakeup count of zero must not be presented as a general elimination of wakeups.

No full working-day sleep/wake and VPN-change soak was performed. The hardware wake simulation and overnight workload tests exercise different conditions.

The allocation strategy creates an owned nonvolatile object per 16 KiB host page. Reservation-only probes measured about 55.7 MiB of kernel metadata for 4 GiB on M1 and 230 MiB for 24 GiB on M5. Metadata can remain in kernel allocator caches after a VM exits. This overhead and the measured cold-start cost accompany the accounting correction; live RAM is never marked volatile or exempt from accounting.

## Raw records and reproduction

The full records are the canonical [M5 share](results/lighter.csv), [guest disk](results/lighter-guest.csv), [amd64](results/lighter-amd64.csv), and [M1 share](results/machines/m1/lighter.csv), [guest disk](results/machines/m1/lighter-guest.csv), [amd64](results/machines/m1/lighter-amd64.csv) CSVs. Their adjacent `.tree` files retain the measured runtime commit and guest-artifact hashes. Documentation and packaging metadata committed afterward do not change the runtime measured here.

The alternating raw CSVs and stamps are retained under [M1](results/variance/m1/0.4.0-vs-0.4.1-final/) and [M5](results/variance/m5/0.4.0-vs-0.4.1-final/). The M1 directory also contains the separate `steady-m1-abba-*` records. Each numeric-change column compares the median of the two run medians for each version. It is descriptive, not a confidence interval.

The [comparison harness patch](results/variance/0.4.0-vs-0.4.1-final-harness.patch) and [warmed HTTP variant](results/variance/0.4.0-vs-0.4.1-steady-http.patch) apply to `benchmarks/run.sh` at `38bfab4`. Set `COMPARE_BIN`, `COMPARE_CLI`, `COMPARE_GUEST_DIR` and `COMPARE_SHA` to each prebuilt version and its own guest artifacts; set `LIGHTER_BENCH_KERNEL` and `LIGHTER_GUEST_DIR` to the selected artifacts too. Use the CPU/RAM/disk sizes, case sequences, repetitions and warm-up described above, with a fresh VM per run and the order old/new/new/old. The old CLI is the signed 0.4.0 release; both benchmark executables were built with the same host Rust toolchain. Raw stamps distinguish the warmed HTTP variant.
