# 0.4.1 preceding runtime measurements — 5f3223e

The full records below measure runtime `5f3223e4a00073b5a32572b1187b418cb441b9be`, including filesystem event-loss recovery, after all twelve hardware gates passed on both Macs. The M5 has 18 cores and 48 GiB RAM; the M1 has eight cores and 8 GiB RAM. Both run macOS 26.6.2 (25G83), and the host executables were built with Rust 1.98.0. Each workload stage uses a fresh VM, eight vCPUs and a 128 GiB sparse disk. Timing cases use three repetitions after cache warming; memory and power rows describe one workload or sampling window. Workload RAM is 16 GiB on M5 and 4 GiB on M1. Cold-start measurements use the CLI defaults: 12 GiB/nine vCPUs on M5 and 2 GiB/four vCPUs on M1.

The M1 is the controlled release reference: before its full record and before each alternating arm, six samples ten seconds apart had to show no more than 1% CPU for `mds_stores`, `mdworker`, `fseventsd`, `photolibraryd` and `mediaanalysisd`. Other container runtimes were stopped and its wallpaper process paused. Workload-generated filesystem activity during a run is part of the measured sequence; this preflight does not promise exclusive use of every CPU. The M5 is supplementary: a quiet preflight and then a fixed one-core-load preflight both expired without taking any measurements. Its filesystem daemon continued at variable load. Before any M5 samples, the protocol was explicitly changed to an observational full record and alternating sequence, with a baseline minute before the record and every arm, plus host CPU sampling every ten seconds throughout. M5 load was neither quiet nor held constant. Small M5 differences and comparisons against historical sessions cannot establish performance changes. The daily VM and other container runtimes were stopped on both hosts.

Full records retain outliers. Percentages compare published medians across sessions and do not by themselves establish causal changes. The alternating comparisons below check selected rows with the same sequence for each version. The [same-build variation](REPEATABILITY.md) was established on an earlier M1 share-storage build: it is not a universal threshold for this runtime, M5, network, memory or cold-start measurements.

Memory is the macOS task-footprint charge, including compressed memory and host overhead. The accounting fix removes a duplicate charge; these reductions are not equivalent reductions in distinct physical RAM. Independent physical reclamation and the real 24/12/8 GiB builds, including successful container and build recovery after guest OOM, are documented in the [memory investigation](../docs/memory-accounting-2026-09-06.md).

## Full records

### M5 — share

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 6213 | 6221 | +0.1% | lower |
| pnpm-install | ms | 4169 | 4007 | -3.9% | lower |
| yarn-install | ms | 5442 | 5129 | -5.8% | lower |
| ripgrep | ms | 85 | 86 | +1.2% | lower |
| find-walk | ms | 88 | 102 | +15.9% | lower |
| copy-tree | ms | 3751 | 3661 | -2.4% | lower |
| rm-rf | ms | 2508 | 2376 | -5.3% | lower |
| cpu-sha256 | ms | 3002 | 2989 | -0.4% | lower |
| container-start | ms | 140 | 181 | +29.3% | lower |
| watch-latency | ms | 2 | 2 | +0.0% | lower |
| memory-peak | MiB | 5423 | 3895 | -28.2% | lower |
| memory-after-15s | MiB | 1143 | 990 | -13.4% | lower |
| memory-after-60s | MiB | 1174 | 1003 | -14.6% | lower |
| net-tcp-egress | Mbit/s | 99299 | 95661 | -3.7% | higher |
| net-tcp-egress-r | Mbit/s | 93903 | 86297 | -8.1% | higher |
| net-tcp-port | Mbit/s | 91547 | 85745 | -6.3% | higher |
| net-tcp-port-r | Mbit/s | 81717 | 97358 | +19.1% | higher |
| net-udp | Mbit/s | 5136 | 5107 | -0.6% | higher |
| net-connect-rate | connections/s | 17026 | 17163 | +0.8% | higher |
| net-http-p99 | µs | 173 | 178 | +2.9% | lower |
| net-http-latency | µs | 57 | 65 | +14.0% | lower |
| net-dns | µs | 41 | 40 | -2.4% | lower |
| power-cpu-ms-per-s | ms CPU/s | 3 | 4 | +33.3% | lower |
| power-wakeups-per-s | wakeups/s | 57 | 63 | +10.5% | lower |
| power-pkg-idle-wakeups-per-s | wakeups/s | 2 | 0 | -100.0% | lower |
| boot-docker | ms | 364 | 1248 | +242.9% | lower |
| boot-first-container | ms | 507 | 1405 | +177.1% | lower |
| memory-idle | MiB | 359 | 310 | -13.6% | lower |

### M5 — guest

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 4532 | 4548 | +0.4% | lower |
| pnpm-install | ms | 1186 | 1155 | -2.6% | lower |
| yarn-install | ms | 4036 | 4002 | -0.8% | lower |
| ripgrep | ms | 80 | 81 | +1.2% | lower |
| find-walk | ms | 92 | 102 | +10.9% | lower |
| copy-tree | ms | 873 | 942 | +7.9% | lower |
| rm-rf | ms | 376 | 400 | +6.4% | lower |

### M5 — amd64

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 8966 | 9025 | +0.7% | lower |
| pnpm-install | ms | 2896 | 2675 | -7.6% | lower |
| cpu-sha256 | ms | 4146 | 4177 | +0.7% | lower |
| container-start | ms | 149 | 146 | -2.0% | lower |

### M1 — share

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 11728 | 11078 | -5.5% | lower |
| pnpm-install | ms | 6628 | 5548 | -16.3% | lower |
| yarn-install | ms | 13414 | 11173 | -16.7% | lower |
| ripgrep | ms | 180 | 189 | +5.0% | lower |
| find-walk | ms | 100 | 124 | +24.0% | lower |
| copy-tree | ms | 5493 | 4996 | -9.0% | lower |
| rm-rf | ms | 2695 | 2575 | -4.5% | lower |
| cpu-sha256 | ms | 6281 | 6275 | -0.1% | lower |
| container-start | ms | 201 | 254 | +26.4% | lower |
| watch-latency | ms | 2 | 2 | +0.0% | lower |
| memory-peak | MiB | 4525 | 3073 | -32.1% | lower |
| memory-after-15s | MiB | 947 | 731 | -22.8% | lower |
| memory-after-60s | MiB | 944 | 726 | -23.1% | lower |
| net-tcp-egress | Mbit/s | 56108 | 56941 | +1.5% | higher |
| net-tcp-egress-r | Mbit/s | 46735 | 50297 | +7.6% | higher |
| net-tcp-port | Mbit/s | 45367 | 49096 | +8.2% | higher |
| net-tcp-port-r | Mbit/s | 54227 | 54735 | +0.9% | higher |
| net-udp | Mbit/s | 4990 | 4885 | -2.1% | higher |
| net-connect-rate | connections/s | 14731 | 9988 | -32.2% | higher |
| net-http-p99 | µs | 216 | 238 | +10.2% | lower |
| net-http-latency | µs | 127 | 130 | +2.4% | lower |
| net-dns | µs | 130 | 129 | -0.8% | lower |
| power-cpu-ms-per-s | ms CPU/s | 7 | 7 | +0.0% | lower |
| power-wakeups-per-s | wakeups/s | 58 | 55 | -5.2% | lower |
| power-pkg-idle-wakeups-per-s | wakeups/s | 1 | 2 | +100.0% | lower |
| boot-docker | ms | 466 | 640 | +37.3% | lower |
| boot-first-container | ms | 649 | 814 | +25.4% | lower |
| memory-idle | MiB | 279 | 230 | -17.6% | lower |

### M1 — guest

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 8177 | 7486 | -8.5% | lower |
| pnpm-install | ms | 1810 | 1658 | -8.4% | lower |
| yarn-install | ms | 7953 | 7779 | -2.2% | lower |
| ripgrep | ms | 203 | 128 | -36.9% | lower |
| find-walk | ms | 124 | 121 | -2.4% | lower |
| copy-tree | ms | 4433 | 3121 | -29.6% | lower |
| rm-rf | ms | 630 | 564 | -10.5% | lower |

### M1 — amd64

| Metric | Unit | 0.4.0 | 0.4.1 | Numeric change | Better |
|---|---|---:|---:|---:|---|
| npm-install | ms | 17090 | 14960 | -12.5% | lower |
| pnpm-install | ms | 4178 | 3788 | -9.3% | lower |
| cpu-sha256 | ms | 7197 | 7190 | -0.1% | lower |
| container-start | ms | 201 | 184 | -8.5% | lower |

## Alternating comparisons

Each host runs old/new/new/old with a fresh VM and three repetitions per timing. Both hosts repeat the seven share-storage cases, container start, connection rate, HTTP latency and cold boot; M5 also repeats all four TCP directions. This reduced sequence differs from the full record. In particular, the copy case here runs on the host share; the earlier `38bfab4` M5 comparison ran on guest disk.

Each cell is a run median. Numeric change compares the median of the two run medians per version. Two runs per version do not establish a confidence interval or a small universal speedup. Both versions use the same harness, CPU/RAM/disk settings and host Rust toolchain. The old CLI uses the 0.4.0 release executable; the common harness re-signs private CLI copies ad hoc with the hypervisor entitlement before boot measurements; each version uses its own guest artifacts. The soft descriptor limit is set to 10,240 while the original hard limit is preserved. Earlier `38bfab4` comparisons lowered both limits; those are historical evidence with a different method.

### M1 alternating comparison

| Metric | Unit | Old 1 | New 1 | New 2 | Old 2 | Numeric change |
|---|---|---:|---:|---:|---:|---:|
| npm-install | ms | 11394 | 10821 | 10825 | 12362 | -8.9% |
| pnpm-install | ms | 6248 | 5796 | 6297 | 7196 | -10.0% |
| yarn-install | ms | 13346 | 10600 | 11094 | 12732 | -16.8% |
| ripgrep | ms | 322 | 184 | 338 | 442 | -31.7% |
| find-walk | ms | 126 | 141 | 125 | 99 | +18.2% |
| copy-tree | ms | 6336 | 6191 | 6297 | 5481 | +5.7% |
| rm-rf | ms | 2585 | 2562 | 2543 | 2590 | -1.4% |
| container-start | ms | 189 | 198 | 199 | 192 | +4.2% |
| net-connect-rate | connections/s | 14289 | 11098 | 10076 | 9662 | -11.6% |
| net-http-p99 | µs | 231 | 267 | 718 | 457 | +43.2% |
| net-http-latency | µs | 129 | 132 | 179 | 132 | +19.2% |
| boot-docker | ms | 450 | 640 | 637 | 445 | +42.7% |
| boot-first-container | ms | 631 | 814 | 816 | 622 | +30.1% |
| memory-idle | MiB | 283 | 246 | 236 | 276 | -13.8% |

### M5 alternating comparison

| Metric | Unit | Old 1 | New 1 | New 2 | Old 2 | Numeric change |
|---|---|---:|---:|---:|---:|---:|
| npm-install | ms | 6379 | 6266 | 6396 | 6497 | -1.7% |
| pnpm-install | ms | 4120 | 3817 | 3870 | 3981 | -5.1% |
| yarn-install | ms | 5205 | 5240 | 5423 | 5649 | -1.8% |
| ripgrep | ms | 86 | 81 | 91 | 104 | -9.5% |
| find-walk | ms | 95 | 89 | 89 | 98 | -7.8% |
| copy-tree | ms | 3609 | 3177 | 3494 | 4230 | -14.9% |
| rm-rf | ms | 2470 | 2366 | 2355 | 2368 | -2.4% |
| container-start | ms | 176 | 153 | 163 | 148 | -2.5% |
| net-tcp-egress | Mbit/s | 102371 | 98328 | 99783 | 97509 | -0.9% |
| net-tcp-egress-r | Mbit/s | 90635 | 89481 | 92510 | 90719 | +0.4% |
| net-tcp-port | Mbit/s | 86941 | 90300 | 89314 | 88883 | +2.2% |
| net-tcp-port-r | Mbit/s | 94532 | 98675 | 96325 | 95324 | +2.7% |
| net-connect-rate | connections/s | 16822 | 16190 | 16336 | 17316 | -4.7% |
| net-http-p99 | µs | 179 | 198 | 134 | 159 | -1.8% |
| net-http-latency | µs | 57 | 65 | 60 | 57 | +9.6% |
| boot-docker | ms | 527 | 1217 | 1283 | 776 | +91.9% |
| boot-first-container | ms | 666 | 1362 | 1433 | 954 | +72.5% |
| memory-idle | MiB | 348 | 299 | 304 | 362 | -15.1% |

## M1 warmed HTTP follow-up

One candidate run after the share-storage and connection-churn sequence had a 179 µs HTTP median, against 132 µs in the other candidate run. A separate old/new/new/old comparison starts fresh VMs, omits those preceding workloads, sends 10,000 untimed HTTP requests, waits two seconds, then measures five batches of 2,000 requests over kept-alive connections. The M1 quiet preflight is applied before every arm. These samples characterize warmed HTTP; they neither replace the original sequence nor isolate which preceding activity caused its variability.

| Metric | Unit | Old 1 | New 1 | New 2 | Old 2 | Numeric change |
|---|---|---:|---:|---:|---:|---:|
| net-http-latency | µs | 122 | 127 | 126 | 121 | +4.1% |
| net-http-p99 | µs | 197 | 222 | 221 | 194 | +13.3% |

## Interpretation

The accounting correction is established by coherent host/guest access and independent physical-page tests, not by a smaller number in a benchmark table. The full M1 record reports a 32.1% lower peak footprint than the published 0.4.0 record; this is a reduction in the macOS charge, not equivalent physical-RAM savings.

Cold start has a clear cost. The controlled M1 alternating comparison increased Docker readiness from 447.5 to 638.5 ms (+191 ms, +42.7%) and first-container readiness from 626.5 to 815 ms (+188.5 ms). Creating and initializing owned mappings adds work that scales with the RAM ceiling. These version comparisons include all release changes and do not isolate every contributor.

M1 npm and yarn installs were faster in both candidate runs: npm medians were 10,821/10,825 ms against 11,394/12,362 ms (-8.9% by the paired medians), and yarn 10,600/11,094 against 13,346/12,732 ms (-16.8%). These are measured changes for this machine and sequence. Pnpm's paired change was -10.0%, with overlapping old/new run medians. Removal changed by -1.4%. The earlier same-build storage variation is useful context, not a universal significance threshold for these new comparisons.

The M1 share metadata walk changed from paired medians of 112.5 to 133 ms (+20.5 ms, +18.2%); both the historical full record and this alternating comparison show a cost. Individual old/new run medians overlap, so the paired percentage should not imply every run is slower. Share copy changed by +5.7%, with overlapping run medians. Search varied substantially: every arm's first timed sample took about 6.6 seconds, while subsequent samples were much shorter. Its -31.7% paired difference is not a stable speedup claim. All samples remain in the raw record.

M1 connection setup changed by -11.6% using paired medians, with large variation among samples and between the old-version runs (14,289 and 9,662 connections/s). This counts client TCP handshakes followed immediately by close, not completed application requests. Separate capacity regressions check successful HTTP and resource drainage after bursts. Post-storage HTTP medians were 129/132 µs for old and 132/179 µs for new (+19.2% by the paired medians). The separate warmed comparison instead measured 121.5 to 126.5 µs (+5 µs, +4.1%), with both candidate run medians higher. Its median-of-run-p99 values rose from 195.5 to 221.5 µs (+26 µs). These characterize warmed HTTP under this test; they do not explain which preceding activity produced the original sequence's variability.

The M5 full record reports a 28.2% lower peak footprint than the historical 0.4.0 record, again a change in accounting rather than equivalent physical-RAM savings. Under variable host load, the alternating M5 Docker-readiness medians changed from 651.5 to 1250 ms (+598.5 ms); first-container readiness changed from 810 to 1397.5 ms (+587.5 ms). Old-version readiness medians differed substantially (527/776 ms), and one individual old start took 1810 ms. These observations are consistent with an initialization cost, but this M5 sequence cannot isolate its exact size from host interference. The controlled M1 comparison above provides firmer evidence of the cost.

M5 paired changes were npm -1.7%, pnpm -5.1%, yarn -1.8%, share copy -14.9%, container start -2.5%, TCP directions -0.9% to +2.7%, and connection setup -4.7%. The two old share-copy medians were 3609 and 4230 ms; ordinary variation is visible within the same version. Median HTTP changed from 57 to 62.5 µs (+5.5 µs), while the candidate run medians differed (65/60 µs). The table retains the arithmetic; variable host load prevents treating these small differences, or the larger copy percentage, as established gains. Guest-disk copy is a separate case: the final full record is 942 ms, compared with 873 ms historically; the current alternating comparison measures share copy and does not resolve that guest-disk difference.

File-change latency is also retained without hiding the slow first sample: the M5 full sequence measured 1340/2/2 ms and M1 423/2/2 ms. The median alone does not describe host event-delivery delays. Separate controlled tests verify recovery from lost notifications, including open and mapped files; the benchmark timing samples do not prove a fixed latency bound.

Power and memory rows are single measurement windows. A rounded package-wakeup count of zero on M5 is not evidence of a general elimination of wakeups.

The allocation strategy creates an owned nonvolatile object per 16 KiB host page. Reservation-only probes measured about 55.7 MiB of kernel metadata for 4 GiB on M1 and 230 MiB for 24 GiB on M5. Metadata can remain in kernel allocator caches after VM exit. This overhead and the measured cold-start cost accompany the accounting correction; live RAM is never marked volatile or exempt from accounting.

No full working-day sleep/wake and VPN-change soak was performed. Hardware wake simulation and overnight workload tests exercise different conditions.

## Raw records and reproduction

The full records are the archived [M5 share](results/archive/5f3223e/m5/lighter.csv), [guest disk](results/archive/5f3223e/m5/lighter-guest.csv), [amd64](results/archive/5f3223e/m5/lighter-amd64.csv), and [M1 share](results/archive/5f3223e/m1/lighter.csv), [guest disk](results/archive/5f3223e/m1/lighter-guest.csv), [amd64](results/archive/5f3223e/m1/lighter-amd64.csv) CSVs. Adjacent `.tree` files retain the measured runtime commit and artifact hashes. Subsequent documentation, test-only isolation and contract checks, and packaging metadata do not change the production runtime measured here.

Sanitized CPU observations are retained beside each host’s alternating records as `cpu-observations.csv`: `ps` samples every ten seconds, labelled by phase. 100% denotes one core. The listed-process sum includes the benchmark processes and is not a background-only or total-machine measurement. A blank filesystem-daemon value means it was outside the sampled top 20 processes, not zero CPU. Only timestamps, phase labels and numeric CPU readings are published. The separate warmed HTTP follow-up used per-arm quiet preflight but no continuous trace. The alternating CSVs and stamps are retained under [M1](results/variance/m1/0.4.0-vs-0.4.1-reset/) and [M5](results/variance/m5/0.4.0-vs-0.4.1-reset/). The [harness patch](results/variance/0.4.0-vs-0.4.1-reset-harness.patch) applies to `benchmarks/run.sh` at `5f3223e`. Set `COMPARE_BIN`, `COMPARE_CLI`, `COMPARE_GUEST_DIR` and `COMPARE_SHA` to the selected prebuilt version and its own artifacts, and set `LIGHTER_BENCH_KERNEL` and `LIGHTER_GUEST_DIR` accordingly. Use the case sequences, three repetitions and settings above, with old/new/new/old ordering.

The M1 raw directory also retains the separate `reset-steady-m1-abba-*` warmed-HTTP records. Its [harness variant](results/variance/0.4.0-vs-0.4.1-reset-steady-http.patch) also applies directly to `benchmarks/run.sh` at `5f3223e`, with the warming and five repetitions described above. The [earlier runtime report](RELEASE-0.4.1-38bfab4.md) preserves the preceding full records, alternating comparisons and warmed HTTP follow-up, with their original scope and limitations. Those measurements do not substitute for the final runtime results above.
