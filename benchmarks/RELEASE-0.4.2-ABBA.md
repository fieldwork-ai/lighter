# 0.4.2 M1 alternating install comparison

The three full M1 suites showed higher host-share install times against a single earlier 0.4.1 baseline. This follow-up checks those cases in alternating order on the same M1: **0.4.1 → 0.4.2 → 0.4.2 → 0.4.1**. All four arms completed.

Each arm starts a fresh VM with eight vCPUs, 4 GiB guest RAM and a 128 GiB sparse data disk. It uses the same host-share fixture, an untimed warm-up, and three ordered repetitions for npm, pnpm and yarn. Both benchmark binaries were built and signed before the initial quiet preflight. There is a twenty-second gap after each arm and no daemon reset.

The 0.4.1 benchmark was rebuilt from source `5f48cf03` and uses the guest payload extracted from the published 0.4.1 archive. The 0.4.2 benchmark was retained from the qualified runtime at source `d0fbd114` and uses its matching guest payload. The harness is the current benchmark runner with explicit prebuilt binary, guest and source-stamp selection; no compilation occurs during the arms. [Raw repetitions, artifact hashes and the exact protocol](results/releases/0.4.2/abba/) are retained.

## Results

Each cell is the median of three timings, in milliseconds. The relative change compares the geometric mean of the two arm medians for each version; this preserves the multiplicative scale of timing ratios.

| Workload | 0.4.1 A1 | 0.4.2 B1 | 0.4.2 B2 | 0.4.1 A2 | Relative change |
|---|---:|---:|---:|---:|---:|
| npm-install | 11400 | 11377 | 10963 | 11197 | -1.15% |
| pnpm-install | 6532 | 6525 | 6515 | 6876 | -2.71% |
| yarn-install | 10129 | 11195 | 9778 | 10289 | +2.49% |

The larger apparent slowdown in the earlier cross-session comparison did not reproduce consistently. In particular, the two 0.4.2 yarn medians moved in opposite directions relative to the surrounding 0.4.1 controls. These small aggregate differences do not establish general speedups or a regression rate: there are only two arms per version, and residual cache, thermal and background variation remain possible.

The original [full-suite comparison](RELEASE-0.4.2.md), including its higher host-share timings, remains intact. This follow-up supplements it rather than replacing the baseline after seeing the outcome.

The same fseventsd PID 81590 was observed throughout the follow-up. Its sampled physical footprint peaked at 6.09 MiB. No historical daemon failure was reproduced.
