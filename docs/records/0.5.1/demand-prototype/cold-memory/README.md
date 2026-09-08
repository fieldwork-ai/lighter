# Cold 2 GiB allocation trade-off

ABBA, three cold starts per arm, 8 vCPUs / 16 GiB guest RAM, same digest-recorded
Node image in each host comparison. A 2 GiB Buffer is filled and sampled every
4 KiB to verify its contents; a fresh container repeats the allocation 15 seconds
later. No repetition is discarded. Quiet-host guards and competing-VM checks
are retained. The two hosts use separate frozen prototype builds.

Geometric means of arm medians:

| Host | Component | Background | Full demand | Change |
|---|---|---:|---:|---:|
| M1 | cli_start_ms | 1652.8 ms | 775.3 ms | -877.4 ms (-53.1%) |
| M1 | first_allocation_ms | 411.0 ms | 677.3 ms | +266.3 ms (+64.8%) |
| M1 | second_allocation_ms | 423.9 ms | 429.0 ms | +5.1 ms (+1.2%) |
| M5 | cli_start_ms | 1250.4 ms | 447.2 ms | -803.2 ms (-64.2%) |
| M5 | first_allocation_ms | 198.3 ms | 363.7 ms | +165.4 ms (+83.4%) |
| M5 | second_allocation_ms | 260.8 ms | 210.4 ms | -50.4 ms (-19.3%) |

The first large allocation is slower with demand backing: creation of its host
accounting objects and guest mappings happens on first access. Repeat allocation
does not show the same cost. This is an explicit trade-off, not a claim that all
operations get faster. The guest continues to see its full configured RAM.

`cli_start_ms` ends when `lighter start` returns. It is not the separate Docker
polling metric in the startup records. Allocation time is measured inside Node,
excluding Docker/container/Node launch. Do not sum these components and label
the result a measured end-to-end application startup time. Verification times
are retained separately.

M1: frozen executable at 57456cf, before the exit-counter correction. M5: frozen
A4 executable built from 3136f0e plus the included counter-fix source patch,
equivalent to runtime 6e9b968. M5 workspace HEAD in metadata includes later docs.
Both use 64 KiB preparation batches of independent 16 KiB objects. Kernel/rootfs
hashes, executable fingerprints and exact recorder fingerprints are retained.

The M5 run follows explicit authorization for sole use: the daily VM was stopped
and Colima was already stopped. Earlier M5 correctness records were collected
while the daily VM was still running. These remain prototype measurements, not
signed release-archive qualification.
