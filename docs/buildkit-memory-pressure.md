# BuildKit under a full memory cgroup

The original double-build failure still reproduced on 0.5.3 after a private
VM reboot. Its idle-cache fixes were valid, but did not resolve this workload.
A targeted cache-preservation test was insufficient acceptance evidence.

## Failure and mechanism

Two concurrent native application builds share an 8 GiB BuildKit cgroup with
no swap, four CPUs, and no BuildKit parallelism limit. Their anonymous memory
can exceed that cap. The bad outcome is not an allocation failure: the daemon
spends minutes unresponsive, without OOM resolving the overload, and even
cancelling one client may fail to drain the work promptly.

On the failing kernel, function-graph tracing of BuildKit threads showed
repeated `filemap_fault` → readahead → `filemap_add_folio` → memcg charge →
reclaim. Executable pages were being evicted and faulted back in. Individual
reclaims often made progress, allowing more speculative charges and reclaim
rather than a decisive OOM failure. This is guest page-cache reclaim under a
container limit; it can happen while the guest has free memory elsewhere.

The failure survived disabling the host's voluntary memory-return mechanisms
in the earlier isolation tests. Full preemption, disabling block polling,
removing our block/Btrfs I/O patches, and restoring the previous executable
read-around policy also failed to resolve it. Those controls and the kernel
reversal below identify a guest reclaim problem rather than a necessary
virtio queue failure. They do not identify a single upstream commit explaining
all differences from Colima's kernel and configuration.

## Change

[Kernel patch 0029](../guest/kernel/patches/0029-readahead-no-direct-reclaim.patch)
clears `__GFP_DIRECT_RECLAIM` in `readahead_gfp_mask()`. Speculative readahead
allocations and charges can fail instead of synchronously reclaiming other
pages. Background reclaim stays enabled. The ordinary demand-read and mmap
fault allocation paths retain their reclaim/OOM behavior and fetch the data
actually requested if prefetch cannot provide it. Readahead window sizes,
container limits and the host memory driver are unchanged.

This is a general kernel policy, with no BuildKit detection or memory reserve.
It deliberately permits earlier allocation failure/OOM when the working set
cannot fit. It does not promise that both builds fit, that OOM picks only one
worker, or that pressure never causes a temporary pause.

Upstream has addressed related readahead/memcg stalls before:
[stop readahead after a failed charge](https://github.com/torvalds/linux/commit/0fd44ab213bcfb26c47eedaa0985e4b5dbf0a494)
and [retain mmap miss history on workingset refault](https://github.com/torvalds/linux/commit/5c46d5319bde73075c5f2cefd555426848eb506f).
Both changes are already present in 6.18.49; this patch is an additional local
policy change, not a missing upstream backport.

## Original-workload qualification

The shared M5 ran correctness/stress tests only. An isolated shareless VM used
16 vCPUs and 16 GiB, with the installed signed 0.5.3 executable/rootfs. Only the
kernel changed. The candidate retains the release config and all 28 existing
patches. BuildKit v0.23.2 was pinned to digest
`ddd1ca44b21eda906e81ab14a3d467fa6c39cd73b9a39df1196210edcb8db59e`.
The builder used `memory=8g,memory-swap=8g,cpuset-cpus=0-3,cpu-period=100000,cpu-quota=400000`.

The application source, Node image, dependency cache and two distinct build
arguments were held fixed. Each run rebuilt the builder stage with
`--no-cache-filter builder`. Cold cases stopped the builder, dropped caches
inside this disposable VM, and restarted it before launching both builds.
The page-ownership probe checked that the executable pages belonged inside
the limited cgroup. No image was pushed or deployed.

An independent `buildctl debug workers` process outside the capped cgroup
queried the same daemon, with an eight-second timeout. These are whole-command
timings, not isolated RPC timings or benchmark claims. Samples after OOM or
cancellation are not evidence of responsiveness while both builds overlap.

| Case, in order | Observed outcome |
| --- | --- |
| Candidate, cold | Pre-OOM probes 9–18 ms; one worker OOM-killed, survivor finished in 95.8 s |
| Released kernel reversal, cold | Probes rose through 2.3/3.0/7.3 s to repeated >8 s timeouts; no OOM; still timing out after cancellation; six-minute limit reached |
| Candidate after another VM reboot | One worker OOM-killed; survivor finished in 110.5 s; late probe was after OOM |
| Candidate, another cold pair | Both workers OOM-killed; both clients returned explicit memory errors by 45.6 s; daemon survived |
| Candidate, warm pair on that same daemon | One worker OOM-killed; survivor finished in 126.1 s; one pressure probe took 4.1 s, subsequent probes recovered |
| Subsequent uncached single build | Finished its build stage in 67.9 s, with the same daemon PID/start time and no intervening restart |

All four candidate pairs ended without manual cancellation. Final worker
queries succeeded. This resolves the sustained wedge in the retained original
reproducer; the 4.1 s pause remains an explicit limitation. The two overlapping
builds still exceed the configured budget, so application concurrency bounds
remain useful independently of this kernel fix.

The candidate Image SHA256 is
`d6fda8264ae57151f11c9069966430468d510567547057a5cdd620e8c7be472f`.
Its config is byte-identical to the released config; the production patch and
tested patch apply the same source diff. Raw traces and workload captures stay
local, outside Git and release assets.

## Broader validation

Buffered sequential reads and sequential/permuted mmap faults verified every
byte of a 192 MiB file for two rounds, first in a 512 MiB container and then a
96 MiB no-swap container holding 64 MiB of anonymous ballast. Both completed
without corruption or OOM. The idle-cache regression also passed: live mapped
code stayed resident, the empty engine released cache, and the cold empty VM
returned unused memory.

All twelve M1 hardware gates passed, along with 360 workspace tests,
24 signed hypervisor tests, formatting and Clippy. One full M1 suite with
three repetitions per timed case is being recorded before review. The M5 daily VM remains on released 0.5.3;
no full M5 benchmark or new release publication is part of this qualification.
