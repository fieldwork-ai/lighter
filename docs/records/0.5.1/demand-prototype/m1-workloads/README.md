# M1 matched workload comparison

Same executable and guest payload at 57456cf, 8 vCPUs / 16 GiB guest RAM on an
8 GiB physical M1. This intentionally matches the startup-capacity investigation,
not the standard 4 GiB M1 full-suite profile. All four ABBA arms passed with
three repetitions per timed case, quiet preflight and continuous competing-VM
checks. Images, pinned tools and runtime artifacts are recorded. Warm-up happens
before timed workloads; this does not measure the first cold allocation.

Geometric means of arm medians:

| Case | Background | Full demand | Relative change |
|---|---:|---:|---:|
| npm-install | 11960.0 ms | 11466.4 ms | -4.13% |
| copy-tree | 6444.6 ms | 5314.9 ms | -17.53% |
| cpu-sha256 | 6361.0 ms | 6299.5 ms | -0.97% |
| memory-peak | 3892.2 MiB | 3724.9 MiB | -4.30% |
| memory-after-15s | 628.0 MiB | 585.5 MiB | -6.77% |
| memory-after-60s | 652.0 MiB | 605.9 MiB | -7.06% |

All medians are lower in the demand arms. This is evidence against a workload
regression, not a universal speedup claim. Copy has a large first-repetition
effect in every arm (15.8–18.4 s first, 4.8–6.7 s later); within-arm CV is
63.6–74.1%. Install CV is 1.6–4.3%, CPU CV 0.2–1.6%. No slow repetition was
discarded. Memory values are sampled process footprints, including host
overhead; they are not maxima over every instant or a configured hard cap.

The first cold allocation is investigated separately. This prototype record
does not qualify the existing immutable signed archive or replace the required
full release workload suites.
