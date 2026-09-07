# 0.4.1 release measurements

These full records measure runtime `297f6226c54c5c33f99dd391440146cf8df70baa`, including owned guest-memory backing, guest-aware compression steering, filesystem event-loss recovery and twenty-second notification-backed cache leases. Subsequent release documentation and formula metadata do not change the runtime.

Both Macs run macOS 26.6.2 (25G83), with Rust 1.98.0. M5 has 18 cores and 48 GiB RAM; M1 has eight cores and 8 GiB. Each stage starts a fresh VM with eight vCPUs and a 128 GiB sparse disk. Workload RAM is 16 GiB on M5 and 4 GiB on M1. Timing cases take three ordered repetitions after warming; memory and power rows describe one workload or sampling window. Cold boots use CLI defaults: 12 GiB/nine vCPUs on M5 and 2 GiB/four vCPUs on M1.

M1 is the controlled reference. Before its full record and each comparison arm, six samples ten seconds apart had to show no more than 1% CPU in the monitored filesystem/indexing processes. Other container runtimes remained stopped and its wallpaper process was paused. Workload-generated activity during a run is part of the sequence. M5 is supplementary: its record observes variable filesystem-daemon and background load, with a baseline minute and CPU sampling every ten seconds. It does not claim exclusive or quiet host use.

All samples, including slow first repetitions, are retained. The full-record percentages compare medians across sessions; they are numeric changes, not causal speedup estimates. The controlled comparison below isolates cache profiles within the guest-aware memory policy. The [same-build variation](REPEATABILITY.md) describes an earlier M1 storage protocol and is not a universal tolerance for this runtime, host, network or boot timing.

Task footprint includes compressed memory and host overhead. Removing duplicate accounting is not an equivalent reduction in distinct physical RAM. Independent physical reclamation, charging on reuse, and actual 24/12/8 GiB builds with post-OOM recovery are documented in the [memory investigation](../docs/memory-accounting-2026-09-06.md).

## Full records

### M5 — share

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 6213 | 6460 | +4.0% | lower |
| pnpm-install | ms | 4169 | 3820 | -8.4% | lower |
| yarn-install | ms | 5442 | 5337 | -1.9% | lower |
| ripgrep | ms | 85 | 96 | +12.9% | lower |
| find-walk | ms | 88 | 91 | +3.4% | lower |
| copy-tree | ms | 3751 | 3895 | +3.8% | lower |
| rm-rf | ms | 2508 | 2477 | -1.2% | lower |
| cpu-sha256 | ms | 3002 | 2997 | -0.2% | lower |
| container-start | ms | 140 | 147 | +5.0% | lower |
| watch-latency | ms | 2 | 3 | +50.0% | lower |
| memory-peak | MiB | 5423 | 4085 | -24.7% | lower |
| memory-after-15s | MiB | 1143 | 817 | -28.5% | lower |
| memory-after-60s | MiB | 1174 | 846 | -27.9% | lower |
| net-tcp-egress | Mbit/s | 99299 | 94032 | -5.3% | higher |
| net-tcp-egress-r | Mbit/s | 93903 | 87636 | -6.7% | higher |
| net-tcp-port | Mbit/s | 91547 | 86981 | -5.0% | higher |
| net-tcp-port-r | Mbit/s | 81717 | 94378 | +15.5% | higher |
| net-udp | Mbit/s | 5136 | 5033 | -2.0% | higher |
| net-connect-rate | connections/s | 17026 | 17426 | +2.3% | higher |
| net-http-p99 | µs | 173 | 180 | +4.0% | lower |
| net-http-latency | µs | 57 | 61 | +7.0% | lower |
| net-dns | µs | 41 | 38 | -7.3% | lower |
| power-cpu-ms-per-s | ms CPU/s | 3 | 4 | +33.3% | lower |
| power-wakeups-per-s | wakeups/s | 57 | 57 | +0.0% | lower |
| power-pkg-idle-wakeups-per-s | wakeups/s | 2 | 0 | -100.0% | lower |
| boot-docker | ms | 364 | 1340 | +268.1% | lower |
| boot-first-container | ms | 507 | 1505 | +196.8% | lower |
| memory-idle | MiB | 359 | 303 | -15.6% | lower |

### M5 — guest

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 4532 | 4578 | +1.0% | lower |
| pnpm-install | ms | 1186 | 1139 | -4.0% | lower |
| yarn-install | ms | 4036 | 4130 | +2.3% | lower |
| ripgrep | ms | 80 | 95 | +18.8% | lower |
| find-walk | ms | 92 | 91 | -1.1% | lower |
| copy-tree | ms | 873 | 889 | +1.8% | lower |
| rm-rf | ms | 376 | 390 | +3.7% | lower |

### M5 — amd64

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 8966 | 9222 | +2.9% | lower |
| pnpm-install | ms | 2896 | 2701 | -6.7% | lower |
| cpu-sha256 | ms | 4146 | 4188 | +1.0% | lower |
| container-start | ms | 149 | 154 | +3.4% | lower |

### M1 — share

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 11728 | 10878 | -7.2% | lower |
| pnpm-install | ms | 6628 | 6212 | -6.3% | lower |
| yarn-install | ms | 13414 | 11199 | -16.5% | lower |
| ripgrep | ms | 180 | 431 | +139.4% | lower |
| find-walk | ms | 100 | 126 | +26.0% | lower |
| copy-tree | ms | 5493 | 7416 | +35.0% | lower |
| rm-rf | ms | 2695 | 2616 | -2.9% | lower |
| cpu-sha256 | ms | 6281 | 6283 | +0.0% | lower |
| container-start | ms | 201 | 198 | -1.5% | lower |
| watch-latency | ms | 2 | 2 | +0.0% | lower |
| memory-peak | MiB | 4525 | 4098 | -9.4% | lower |
| memory-after-15s | MiB | 947 | 834 | -11.9% | lower |
| memory-after-60s | MiB | 944 | 828 | -12.3% | lower |
| net-tcp-egress | Mbit/s | 56108 | 57283 | +2.1% | higher |
| net-tcp-egress-r | Mbit/s | 46735 | 49966 | +6.9% | higher |
| net-tcp-port | Mbit/s | 45367 | 49126 | +8.3% | higher |
| net-tcp-port-r | Mbit/s | 54227 | 55421 | +2.2% | higher |
| net-udp | Mbit/s | 4990 | 5018 | +0.6% | higher |
| net-connect-rate | connections/s | 14731 | 15246 | +3.5% | higher |
| net-http-p99 | µs | 216 | 247 | +14.4% | lower |
| net-http-latency | µs | 127 | 132 | +3.9% | lower |
| net-dns | µs | 130 | 120 | -7.7% | lower |
| power-cpu-ms-per-s | ms CPU/s | 7 | 7 | +0.0% | lower |
| power-wakeups-per-s | wakeups/s | 58 | 58 | +0.0% | lower |
| power-pkg-idle-wakeups-per-s | wakeups/s | 1 | 1 | +0.0% | lower |
| boot-docker | ms | 466 | 619 | +32.8% | lower |
| boot-first-container | ms | 649 | 808 | +24.5% | lower |
| memory-idle | MiB | 279 | 242 | -13.3% | lower |

### M1 — guest

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 8177 | 7535 | -7.9% | lower |
| pnpm-install | ms | 1810 | 1667 | -7.9% | lower |
| yarn-install | ms | 7953 | 7814 | -1.7% | lower |
| ripgrep | ms | 203 | 159 | -21.7% | lower |
| find-walk | ms | 124 | 124 | +0.0% | lower |
| copy-tree | ms | 4433 | 3027 | -31.7% | lower |
| rm-rf | ms | 630 | 574 | -8.9% | lower |

### M1 — amd64

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 17090 | 15018 | -12.1% | lower |
| pnpm-install | ms | 4178 | 3767 | -9.8% | lower |
| cpu-sha256 | ms | 7197 | 7134 | -0.9% | lower |
| container-start | ms | 201 | 191 | -5.0% | lower |

## Controlled cache-profile comparison

M1 ran long/twenty/twenty/long leases with the corrected memory policy at `129af505e8a72499ad150a114e19aebe986dd342`. Each arm used a fresh VM, the quiet preflight above, eight vCPUs, 4 GiB RAM, a 128 GiB disk and three repetitions of npm, pnpm, yarn, search, walk, copy, remove and watch. Long means 300-second positive/attribute/directory leases and 30-second negative leases; twenty means 20 seconds for all four. Explicit environment overrides select both profiles, and the guest negotiated notifications. The shipping defaults at `297f622` select the twenty-second profile.

Each cell is a fresh-run median; the percentage compares the median of the two run medians per profile. The three repetitions within a run are ordered cache measurements, not independent fresh runs. Two arms per profile cannot establish a confidence interval or a universal small speed change.

| Metric | Unit | Long 1 | Twenty 1 | Twenty 2 | Long 2 | Numeric change |
|---|---|---:|---:|---:|---:|---:|
| npm-install | ms | 11395 | 11708 | 11667 | 11396 | +2.6% |
| pnpm-install | ms | 5511 | 6226 | 6449 | 6358 | +6.8% |
| yarn-install | ms | 11243 | 10187 | 10578 | 10141 | -2.9% |
| ripgrep | ms | 259 | 369 | 345 | 199 | +55.9% |
| find-walk | ms | 123 | 121 | 124 | 122 | +0.0% |
| copy-tree | ms | 6563 | 6562 | 8145 | 5452 | +22.4% |
| rm-rf | ms | 2529 | 2604 | 2570 | 2559 | +1.7% |
| watch-latency | ms | 2 | 2 | 2 | 3 | -20.0% |

Both twenty-second search medians (369 and 345 ms) exceeded both long-lease medians (259 and 199 ms). The median of run medians rose from 229 to 357 ms: +128 ms, or +55.9%. This is a consistent cost in these four arms, and both twenty-second runs still cleared the unchanged 200% native-speed floor. Twenty seconds is the shortest tested profile that did so in both arms; five and ten seconds failed that floor in the preceding experiments. The shorter leases are retained as a freshness backstop, with this cost explicitly recorded.

Copy rose from 6,007.5 to 7,353.5 ms (+1,346 ms, +22.4%), but one twenty-second run was effectively equal to the slower control (6,562 versus 6,563 ms), while the other took 8,145 ms. This shows a workload cost and considerable run-to-run variation; it is not a claim that every copy becomes 22% slower. Npm was +2.6%, pnpm +6.8%, yarn -2.9%, walk unchanged and remove +1.7%. With only two fresh runs per profile and overlap in several rows, the small differences do not establish general speedups or regressions. Watch medians of 2–3 ms are too quantized for their percentage difference to be useful.

Earlier experiments on `80518f3`, before guest-demand vetoed compression steering, rejected five- and ten-second leases: search medians were 782/762 ms and 1,419/1,176 ms against a 1,233-ms native baseline, failing the unchanged 200% search-speed floor. Twenty-second search medians were 299/203 ms, but copy medians rose to 8,312/9,288 ms against surrounding controls of 5,586/5,380 ms. Those results are preserved and are not pooled with the policy-corrected comparison.

## Reliability and limits

An initial M5 qualification of `297f622` passed eleven hardware gates but failed the watch median: updates took 20.004 s, 525 ms and 2 ms, exceeding the existing 100-ms median budget. The first update demonstrates the independent expiry backstop during a delayed callback; the run remains a failed qualification. A separate controlled probe with no new invalidation refreshed ordinary names and attributes after 20.051 s. Reset and overflow probes retained their long-lease same-mtime, open-file, mmap and truncation assertions. Twenty seconds bounds cache leases, not arbitrary filesystem-operation completion time; mappings and writes preserving both size and mtime still depend on invalidation. See the [event investigation](../docs/filesystem-event-loss-2026-09-07.md).

The accounting correction creates an owned nonvolatile object per 16 KiB host page. Reservation-only probes measured about 55.7 MiB of kernel metadata for 4 GiB on M1 and 230 MiB for 24 GiB on M5. Kernel allocator caches can retain metadata after exit. Cold-start work and this metadata accompany the correction; live RAM is never exempt from accounting. No full working-day sleep/wake and VPN-change soak was performed.

## Raw evidence and reproduction

Canonical full records: [M5 share](results/lighter.csv), [guest disk](results/lighter-guest.csv), [amd64](results/lighter-amd64.csv); [M1 share](results/machines/m1/lighter.csv), [guest disk](results/machines/m1/lighter-guest.csv), [amd64](results/machines/m1/lighter-amd64.csv). Adjacent `.tree` files record source and artifact stamps. [Cache comparisons](results/variance/m1/cache-leases-041/) retain every ordered sample, including rejected profiles.

The [M5 observations](results/variance/m5/041-final/) retain sanitized CPU readings and the initial failed speed-gate sample. CPU rows sum ps-reported percentages over listed processes, including the benchmark; this is not a background-only or total-machine CPU reading. A blank filesystem-daemon value means it was outside the sampled top sixteen processes, not zero CPU.

The profile comparison uses ordinary `benchmarks/run.sh --target lighter --reps 3 --cases "npm-install pnpm-install yarn-install ripgrep find-walk copy-tree rm-rf watch-latency"`, with `BENCH_CPUS=8`, `BENCH_MEMORY_MIB=4096`, `BENCH_DISK_GIB=128`, and `LIGHTER_FS_ATTR_MS`, `LIGHTER_FS_ENTRY_MS`, `LIGHTER_FS_DIR_ENTRY_MS`, `LIGHTER_FS_NEGATIVE_MS` set to the profile values above in milliseconds. Use a unique label and fresh VM for each arm. The M1 full record uses the same settings and default cache profile; the M5 full record changes workload RAM to 16384 MiB and uses the observational host condition described above.

The [preceding runtime report](RELEASE-0.4.1-5f3223e.md) retains the earlier full records, old/new/new/old release comparisons and warmed HTTP follow-up. Its causal interpretations apply to that earlier runtime, not automatically to the final one.

## Later exact-source qualification

The metadata commit `2e2bce5` contains this runtime and these full records. Its M1 qualification passed all twelve hardware gates; M5 passed eleven but failed the watch median with 20,004/445/1 ms. This failure remains part of the evidence. Both hosts passed 321 workspace tests and fifteen signed hypervisor tests. Subsequent diagnostic passes do not replace the failed qualification. [The event-delivery investigation](../docs/filesystem-event-loss-2026-09-07.md) records independent host/VMM timestamps and rejected App Nap/bundle hypotheses. Replacement signing, notarization and actual-archive testing remain pending; the existing prerelease is the earlier `19c0786` candidate.
