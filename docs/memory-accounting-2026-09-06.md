# Build-time memory accounting investigation

**Release requirement:** 0.4.1 must fix the duplicate memory charge. Documenting
it is not sufficient. The fix must preserve physical reclamation of partial
ranges and account for pages correctly when the guest reuses them. The experiments below check each of these requirements.

The daily M5 VM is configured for **24 GiB (24576 MiB)** and sixteen vCPUs.
The initial captures below used 0.4.0, PID 4788, without restarting or
reconfiguring it. Nick subsequently authorized overnight use of both Macs,
including stopping the daily VM for candidate validation. The configured guest RAM is not a limit on the
host process's footprint: host allocations add overhead, and the experiments
below reproduce a duplicate charge for shared guest backing pages.

## Captured application builds

Nick identified `~/git/app/scripts/deploy-app.sh --push-only` as the trigger
for Activity Monitor readings around 24–30 GB. That script still builds the
application, pushes images and uploads assets; only its service roll is skipped.
For diagnosis, two local image builds used its Dockerfile with source-map
upload disabled and a local image tag. Both completed successfully. No registry
push, asset upload or deployment was invoked.

The process's `TASK_VM_INFO` counters were sampled every 200 ms through a
read-only task-name port; guest `/proc/meminfo` and cgroups were sampled about
once per second. The second capture also saved `footprint -j` region summaries
about every two seconds. No debugger was attached. Times below are UTC.

| Capture | Build interval | Peak time | Reported footprint | Internal | Compressed |
|---|---|---|---:|---:|---:|
| 1 | 19:26:22–19:27:19 | 19:26:57.094 | 29.528 GiB | 16.305 GiB | 13.199 GiB |
| 2 | 19:39:03–19:39:59 | 19:39:35.616 | 30.253 GiB | 13.045 GiB | 17.184 GiB |

These are macOS accounting values, not measurements of distinct physical RAM
chips occupied. The compressed counter represents the logical charge for
compressed pages, not their compressed storage size. A smaller allocation/page
table contribution accounts for the remaining difference. At the first peak,
the nearest guest snapshot, 88 ms later, reported 2.98 GiB free, 6.06 GiB
available, 14.30 GiB anonymous pages and 216 MiB ballooned. Separate host and
guest snapshots are not atomic, especially as build workers exit.

The private raw captures are retained locally under
`.logs/041/app-memory-build-{1,2}/` (host CSV, guest snapshots, build log and
timing metadata). They contain application details and are not release assets.

## Minimal reproduction

The existing `lighter-hv` memory-accounting example and earlier worklog entries
already documented excessive guest footprint. This investigation adds a small
coherence and reuse check through the current `GuestMemory` implementation:

```sh
cargo build --release -p lighter-vmm --example guest-host-accounting
scripts/sign.sh target/release/examples/guest-host-accounting
target/release/examples/guest-host-accounting
```

It allocates 64 MiB, lets a tiny guest write every page, then reads those same
pages from the host. It also writes from the host and verifies the value from
the guest, releases half the memory while preserving the other half, reuses
the released pages and verifies the contents again. It does not boot Linux,
run a benchmark or modify the daily VM.

The initial equivalent standalone probe produced:

| Host / backing | Guest wrote 64 MiB | Host then read the same pages |
|---|---:|---:|
| M5, private anonymous | 69,190,184 bytes | 136,331,864 bytes |
| M1, private anonymous | 69,027,264 bytes | 136,169,024 bytes |
| M1, shared anonymous | 69,027,264 bytes | 136,169,024 bytes |

Both mappings see the same values in both directions. Merely reading the
guest-written pages from the host adds approximately another 64 MiB of charge.
Switching from `MAP_PRIVATE` to `MAP_SHARED` does not remove it. This establishes
duplicate accounting of the same backing, independent of the application's
build behaviour or the M5's background load.

Guest-only pages need not appear in the host virtual-map region totals. A
64 MiB guest-only probe had about 66 MiB task footprint but only about 1.9 MiB
in the region summary. Therefore subtracting the `vmmap` or `footprint` region
headline from the task ledger does **not**, by itself, measure an overcount.
The coherence experiment is the evidence for duplication.

Near the second build peak, one region sample reported a 29.84 GiB task
footprint and 6.53 GiB of dirty anonymous host mappings. Their difference is
23.31 GiB, consistent with a guest charge below its 24 GiB configuration plus
a duplicate host-mapping contribution. This is a consistency check, not an
exact measurement of unique memory: sampling is not atomic and the category
also includes non-guest anonymous allocations. Do not add that category's
`swapped` value to `dirty`; the former is included in the latter.

## Physical reclamation cross-check

A standalone 512 MiB variant ran on the quiet M1 after the variance record.
`vm_stat` sampled global physical-page counters at the same stages; the host
page size was 16 KiB. The compressor stayed at 12,568 pages throughout.

| Stage | Global free pages | Global anonymous pages | Task footprint bytes |
|---|---:|---:|---:|
| Baseline | 195252 | 66017 | 1836224 |
| Guest wrote 512 MiB | 162558 | 98790 | 539019520 |
| Host read the same pages | 162561 | 98790 | 1076235584 |
| Released half | 178951 | 82406 | 539101504 |
| Guest reused half | 162570 | 98791 | 807684928 |
| Released all | 195284 | 66024 | 1885312 |

Host reads doubled the charge with no increase in anonymous physical pages.
Partial release reduced anonymous physical pages by exactly 16,384 (256 MiB),
and reuse restored approximately that amount. After full release the host
returned within seven anonymous pages of baseline. This verifies actual
reclamation for this resident-memory test, independently of the task ledger.
It does not reproduce every compressed or fragmented state of the daily VM.
The raw log is `.logs/041/release-physical-m1.log`.

## Candidate allocation methods rejected

Scratch probes tested single large nonvolatile purgeable allocations and named memory
entries with owner ledger accounting. Both avoided the second mapping's
charge, but replacing only half the mapping retained the old object's backing
pages. Guest reuse then increased the charge from about 66 MiB to 98 MiB instead
of returning to the original size. `MADV_FREE` did not provide immediate partial
reclamation. These large-object approaches were rejected.

Do not mark live pages reusable or volatile to lower the displayed number.
The previously diagnosed `MADV_FREE_REUSABLE` path fails to restore the charge
when the guest reuses pages. A valid fix must preserve coherent contents,
discard surrendered partial ranges, and charge newly reused pages correctly.

## 0.4.1: one owned object per host page

The candidate uses `mach_vm_allocate` with `VM_FLAGS_PURGABLE` to create
**nonvolatile** owned objects. Ownership accounts for each physical page once
across its host and guest mappings. Every 16 KiB host page gets its own object:
releasing a partial guest range removes complete objects rather than leaving
pages retained by the untouched part of a large object. Replacement objects
start empty, and their next access is charged normally. Live RAM is never made
volatile, reusable or exempt from footprint accounting.

The production `GuestMemory` example on M5 reported 69,124,624 bytes after guest
writes and 69,173,848 after host reads; releasing half reduced it to 35,586,600,
and guest reuse restored 69,157,440. The signed hypervisor regression now checks
host reads do not duplicate charges through three rounds of alternating
single-page release and reuse, as well as preserving unreleased contents.

A 512 MiB cross-check on M1 used the candidate production allocation and release
paths, with 16 KiB host pages and the compressor fixed at 8,911 pages:

| Stage | Global free pages | Global anonymous pages | Task footprint bytes |
|---|---:|---:|---:|
| Baseline | 65339 | 107474 | 1803456 |
| Guest wrote 512 MiB | 32582 | 140244 | 539003136 |
| Host read the same pages | 32585 | 140244 | 539331904 |
| Released half | 48911 | 123860 | 270633280 |
| Guest reused half | 32530 | 140245 | 539216704 |
| Host read reused pages | 32533 | 140246 | 539364672 |
| Released all | 65308 | 107477 | 1868928 |

Half release returned exactly 16,384 anonymous pages (256 MiB); reuse restored
them. Host reads did not allocate another copy or duplicate the task charge.
Full release returned within three anonymous pages of baseline. Raw evidence:
`.logs/041/release-physical-owned-m1.log`.

Object metadata has a cost. A reservation-only probe creating 262,144 objects
for 4 GiB on M1 took 0.227 s and increased global wired pages by about 55.7 MiB.
On M5, 1,572,864 objects for 24 GiB took 1.004 s and increased global wired pages
by about 230 MiB. These are separate kernel allocations; the probe did not
map them into a VM or touch guest RAM. Kernel zones may retain freed metadata
for reuse. The [release measurements](../benchmarks/RELEASE-0.4.1.md) record the startup cost and full workload results for this tradeoff.

## Smaller configurations and build OOM priority

Two candidate app builds at 24 GiB completed in 54.95 and 57.79 seconds. Their
sampled footprint peaks were 21.144 and 21.360 GiB; the task's lifetime peak
counter reached 21.223 and 21.360 GiB respectively. Both exercised compressed
memory, and footprint fell below 5 GiB afterward. These are observations of
the complete daily environment, not a controlled claim about build speed.

Nick then requested the same workload at 12 and 8 GiB, retaining sixteen
vCPUs and existing daily-driver data. The first 12 GiB run reached a lifetime
peak of 12,281.22 MiB, below its 12,288 MiB setting. It exhausted guest memory,
stopped making progress, and returned to 3,250.86 MiB after build cancellation.
This establishes the observed bound, not successful compilation at that size.
The first 8 GiB run also stayed below its configured size but left a build
worker alive after cancellation. Its post-build Docker health check was
invalidated by an incorrect diagnostic signal and is excluded. The fresh
repeat below replaces that health check.

The smaller runs exposed a separate OOM-priority bug: a BuildKit worker used
`oom_score_adj=-900`, inherited from the protected Docker daemon, while normal
containers used zero. Linux killed small application processes while the build
held most guest memory. Docker's embedded [executor configuration](https://github.com/moby/moby/blob/v28.3.3/builder/builder-next/executor_linux.go)
does not supply an OOM score, and the [OCI contract](https://github.com/opencontainers/runtime-spec/blob/main/config.md)
preserves the inherited value when that field is absent.

The candidate gives BuildKit a small runtime launcher that resets this value
to zero before executing runc. Docker and containerd retain their own protection.
The Docker hardware gate checks both the score inside an actual build and the
engine scores. Fresh 12/8 GiB runs verify failure and recovery under pressure; these results
are not interchangeable with the earlier runs:

| Configured guest RAM | Lifetime process peak | Build outcome | Footprint after 120 s | Docker health |
|---|---:|---|---:|---|
| 12,288 MiB | 12,240.49 MiB | ResourceExhausted after 24.42 s | 3,071.01 MiB | responsive |
| 8,192 MiB | 8,153.28 MiB | ResourceExhausted after 14.47 s | 3,138.99 MiB | responsive |

Neither process's lifetime peak breached its configured RAM in these tests.
The oversized builds failed promptly without manual cancellation; the kernel
selected the build worker at score adjustment zero. These are successful
limit/recovery checks, **not successful application builds** at 12 or 8 GiB.
The full daily environment and sixteen vCPUs were retained. Configuration was
restored byte-for-byte to 24 GiB afterward. Raw captures and health snapshots
are `.logs/041/app-memory-build-owned-oomfixed-{12288,8192}m/`.

The memory setting is a guest RAM ceiling. Host allocations and kernel mapping
metadata add overhead; these observed peaks do not establish an absolute cap
on every possible host-process allocation.

## Final runtime build checks

The frozen runtime candidate `38bfab4`, including the published-connection burst fix, repeated the same local application build at all three sizes. These runs retained the daily data and sixteen vCPUs, sampled the task ledger every 200 ms and checked the task lifetime peak. No region-walking sampler, registry push, asset upload or deployment ran. The lifetime peak catches peaks between periodic samples.

| Configured guest RAM | Lifetime process peak | Build outcome | Footprint after 120 s | Docker health |
|---|---:|---|---:|---|
| 24,576 MiB | 21,517.25 MiB | completed after 58.98 s | 4,314.82 MiB | responsive |
| 12,288 MiB | 12,256.34 MiB | ResourceExhausted after 23.78 s | 3,019.06 MiB | responsive |
| 8,192 MiB | 8,157.50 MiB | ResourceExhausted after 20.31 s | 3,028.87 MiB | responsive |

All three lifetime peaks stayed below their configured guest sizes. The 24 GiB run included up to 12,331.80 MiB of compressed-memory charge; the smaller runs did not require host compression. The 12 and 8 GiB results verify bounded observations and recovery under guest OOM, not successful builds at those sizes. The original 24 GiB configuration was restored byte-for-byte afterward.

Private raw evidence is `.logs/041/app-memory-build-owned-burstfixed-{24576,12288,8192}m/`, with the summary in `.logs/041/memory-matrix-burst-fixed-results.json`. These application-specific captures are not release assets.

## Status

Runtime `38bfab4` removes duplicate accounting and preserves actual partial
reclamation in controlled tests on both Macs. The application builds exercise
compressed memory and recovery when the workload cannot fit. All twelve hardware
gates, 316 workspace tests and 15 signed hypervisor tests passed on each Mac.
Complete host-share, guest-disk and amd64 records also passed on both machines.
The [release measurements](../benchmarks/RELEASE-0.4.1.md) retain timings,
source/artifact stamps, matched comparisons and the costs of the allocation strategy.
These observations do not turn the guest RAM setting into an absolute cap on
all host allocations.

Apple's public [Hypervisor mapping contract](https://developer.apple.com/documentation/hypervisor/hv_vm_map(_:_:_:_:))
describes the host allocation backing guest RAM. The open-source
[task ledger formula](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/task.c)
and [memory object ownership rules](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/vm/vm_object.c)
help interpret these experiments; public XNU source alone does not expose the
entire AppleHV implementation.
