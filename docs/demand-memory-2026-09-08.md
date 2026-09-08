# Demand-backed RAM investigation

Status: hybrid preparation is now the 0.5.1 candidate. Pure demand at `b303dc2`
was evaluated but left first-use preparation in workloads and split reclamation
into 256 KiB calls. The historical results below describe those prototypes,
not the hybrid candidate. Publication remains on hold pending qualification.

The hybrid default offers guest capacity immediately and runs one worker over
base RAM, then hotplug RAM. First CPU/device accesses prepare any chunk ahead
of that worker through the same synchronized operation. Docker readiness does
not wait for the worker. Preparation establishes independent backing objects
and mappings; it does not touch all configured RAM into physical residency.

After a region is completely prepared, checked host accesses bypass per-chunk
preparation checks and reclamation uses the original contiguous-range path.
During preparation, reclamation skips untouched chunks under the preparation
locks. Ready chunks remain ready after reclamation and are never overwritten
by the worker. Completion is published only after preparation locks drain.
The worker checks shutdown between batches and is joined before VM teardown.

Diagnostic modes are explicit: `LIGHTER_DEMAND_RAM=1 LIGHTER_BACKGROUND_RAM=0`
selects pure demand; `LIGHTER_DEMAND_RAM=0 LIGHTER_BACKGROUND_RAM=1` selects the
legacy background path; both zero select eager preparation. `LIGHTER_DEMAND_BASE=0`
keeps the initial quarter eager in demand modes. The boot recorder's `hybrid`
mode sets all three flags to one, and `demand-all` explicitly disables the worker.

Each 256 KiB preparation batch contains sixteen independent 16 KiB owned Mach
objects on Apple Silicon. The batch size does not change allocation ownership
or partial reclamation. Stage-2 translation faults prepare backing and retry
the same instruction. Checked host memory accessors prepare device spans before
returning pointers. Striped locks serialize first access; published chunks are
never replaced by another preparation attempt. Allocation or mapping failure
terminates the VM process. Reporting untouched RAM does not allocate it.

The existing full-online-memory gate remains in guest init. This advertises real
guest capacity without eagerly creating every host accounting object; it does
not falsify MemTotal or promise a hard cap on all host process overhead.

## Controlled M1 measurements

These initial comparisons use the earlier 64 KiB batching prototype.

The same prototype binary and guest payload run with 8 vCPUs, 16 GiB RAM and
a saved stopped Alpine container. Each arm has three cold starts. Quiet-host
guards require six samples at most 5% total CPU, ten seconds apart; competing
VMs invalidate a run. All repetitions are retained. Figures are geometric
means of arm medians, not isolated kernel boot times.

| Preparation | Docker ready | First container complete |
|---|---:|---:|
| Background | 1,616.4 ms | 1,794.8 ms |
| Demand, initial quarter eager | 954.3 ms (−41.0%) | 1,136.6 ms (−36.7%) |
| Demand, including initial quarter | 690.5 ms (−57.3%) | 868.5 ms (−51.6%) |

Order: background, demand, full demand, full demand, demand, background.
An earlier hotplug-only prototype also passed ABBA and BAAB, reducing Docker
readiness by 41.0% and 42.4% respectively. These comparisons are against the
background mode in the same executable, not a final signed release comparison.
[Raw repetitions, per-arm variance, guards and artifact hashes](records/0.5.1/demand-prototype/)
retain both experiments, including their exact source patches against dff2632.
The recorder itself does not enforce quiet; the outer guard records do.

## Correctness and remaining qualification

The earlier M5 prototype tests used isolated homes while the user's daily VM remained running. Full
demand backing passes new/restored-container MemTotal checks at 8, 12, 16 and
32 GiB (within 4 KiB of eager backing), a twice-touched and verified 2 GiB working
set, volume persistence across VM restart, amd64 execution and the two-node
kind suite including restart/persistence. An earlier hotplug-only variant
also passed 820 Docker/HTTP stream checks. A first 16 GiB attempt failed the
disk-space preflight before starting a VM; its logs are retained separately.

[All eleven M5 functional gates](records/0.5.1/demand-prototype/m5-correctness/)
pass at 57456cf, excluding the speed gate as instructed. The 3 GiB ballast
check reaches a 3,573 MiB footprint and returns to 269 MiB within five seconds.
343 workspace tests and 22 signed hardware tests pass. Hardware coverage includes
sparse device access, four simultaneous guest CPUs, instruction retry, partial
reclamation, reuse without duplicate accounting, and injected allocation/map
failures that must abort the entire disposable VM process.

[Matched M1 workload arms](records/0.5.1/demand-prototype/m1-workloads/)
show no median regression in installs, copying or CPU work, and slightly lower
peak/settled footprints. The copy case has a large first-repetition effect, so
these are not general speedup claims. The [cold-allocation comparison](records/0.5.1/demand-prototype/cold-memory/)
quantifies the trade-off: the first 2 GiB fill adds 266 ms on M1 and 165 ms on
M5 with 64 KiB preparation batches. Repeat allocations do not show that penalty.
A bounded [64/256/64 KiB comparison](records/0.5.1/demand-prototype/batch-size/) then measured the first 2 GiB fill at
689 → 644 ms (geometric means of arm medians), with CLI startup approximately
701 ms in both. All 22 hardware tests also pass with 256 KiB batches. This
reduces first-use overhead without changing allocation ownership; 256 KiB is
selected for the default candidate. The 64 KiB measurements remain labelled
as prototypes rather than being rewritten as final-candidate figures.
The current release path unmaps/remaps each prepared chunk separately;
its cost also needs measurement. The user has now granted sole M5 use and authorized full benchmarks; its daily
VM is stopped and its configuration is preserved. Earlier M5 correctness
records used the shared host. A changed release runtime requires fresh qualification,
signing, notarization and package checks.

## Hybrid validation protocol

Use one M1 observation per version for host-share npm, pnpm, yarn, copy and
deletion, alternating version order across workloads. More than 5% slowdown
triggers two additional observations of that workload only. Record three
interleaved cold starts per mode (0.5.0, pure demand, hybrid), including background
completion timing. A hybrid startup median over 10% slower than pure demand
requires investigation. These thresholds trigger investigation, not statistical
significance claims.

Freeze the final runtime before one full suite per host, three repetitions per
timed case. M5 latency measurements require renewed quiet-machine clearance.
`scripts/records/record-release.py` now runs that single suite, or the focused
comparison when supplied `--compare-bin` and `--compare-guest`. There is no outer
suite repetition or automatic variance/ABBA sequence. Earlier completed suites
remain historical evidence, not qualification of the hybrid implementation.

### Hybrid correctness at `11ddb88`

[M5 correctness records](records/0.5.1/hybrid/m5-correctness/) retain 343 passing
workspace tests (serial), 24 signed hypervisor tests and ten functional gates.
The new tests exercise concurrent background preparation and guest/device access,
reclamation during preparation, the completion transition, data preservation,
whole-range reclamation calls and physical release/recharge. The initial parallel
workspace run's filesystem global-counter failure is retained alongside its
passing isolated and serial reruns; no filesystem code was changed to hide it.

Fresh and automatically restored containers see the same MemTotal as eager mode
within 4 KiB at 8, 12, 16 and 32 GiB. These are development-binary correctness
checks with the user's Colima left running, not controlled M5 performance results
or qualification of a notarized shipping archive. The speed and memory/idle
hardware gates remain separate pending work.

The speed gate can consume the completed suite directly:
`bash scripts/gates/m5-speed.sh --check-records native.csv lighter.csv lighter-boot.log`.
The caller supplies the validated full-suite CSV and its matching boot log, plus
a fresh native baseline. This applies the existing speed, visibility and
descriptor-budget thresholds without launching another benchmark or rewriting
the published report. Its evaluator is checked with passing and deliberately
failing visibility fixtures. The memory/idle gate remains a separate stress test.

### Focused M1 screening

[Matched raw records](records/0.5.1/hybrid/m1-focused/) compare 0.5.0 with hybrid
`11ddb88`, using 8 CPUs and 4 GiB guest RAM. There is one measured observation
per workload per version, with identical fixture setup and version order
alternated across workloads. Quiet-host checks precede each arm. Paused Apple
background processes were restored after the controller exited.

| Host-share workload | 0.5.0 | Hybrid | Time change |
|---|---:|---:|---:|
| npm install | 11,041 ms | 11,173 ms | +1.2% |
| pnpm install | 5,860 ms | 5,883 ms | +0.4% |
| yarn install | 10,589 ms | 10,219 ms | −3.5% |
| Tree copy | 15,925 ms | 15,921 ms | −0.03% |
| Tree deletion | 2,745 ms | 2,744 ms | −0.04% |

No case crosses the predeclared +5% investigation threshold, so no additional
screening samples are triggered. This does not establish statistical equivalence
or a speedup. It shows the earlier historical slowdown was not reproduced in
this matched screening of the hybrid candidate. Copying is a first observation
on a newly materialized tree, not the median of the earlier full-suite repeats.
The final full suite remains required.

### Hybrid startup contention

[Three interleaved M1 cold starts per mode](records/0.5.1/hybrid/m1-boot/)
at 16 GiB and 8 CPUs measured Docker-ready medians of 2,502 ms for 0.5.0,
621 ms for pure demand and 844 ms for hybrid `11ddb88`. Hybrid's 36% penalty
over pure demand exceeds the investigation threshold. The 0.5.0 observations
were 1,769, 2,502 and 2,645 ms; this spread remains visible in the records.
An earlier attempt with a respawned media-analysis process was invalidated
before selecting these results. No full hybrid suite has been started.

A [separate utility-priority trial](records/0.5.1/hybrid/m1-utility-trial/)
measured 849 ms at ordinary priority and 774 ms at utility priority, three
interleaved cold starts each. This modest reduction does not close the gap
to pure demand. The priority change was removed rather than selected for release.

A diagnostic one-second sample of the ordinary preparation worker found 364
of 702 stack samples in allocation, 265 in hypervisor mapping (including 41
waiting for its internal lock), and 72 in `task_self_trap`. This is a stack
sample distribution, not an attribution of Docker startup latency. Inspection
found that our Rust FFI called `mach_task_self()` for every backing page,
whereas Apple's `mach/mach_init.h` defines that C spelling as the cached
`mach_task_self_` value. Using the same cached port removes one unnecessary
kernel trap per page without changing ownership, preparation or reclamation.

The [cached-port comparison](records/0.5.1/hybrid/m1-cached-task/) measured
Docker-ready medians of 832 ms before and 839 ms after (+0.8%), while background
completion fell from 1,737 to 1,605 ms (−7.6%). There are three interleaved cold
starts per version. Removing redundant calls saves preparation work but does
not resolve foreground startup contention. All 343 workspace and 24 signed
hypervisor tests pass with this change.

A [2 MiB worker-mapping trial](records/0.5.1/hybrid/m1-mapping-trial/) kept
256 KiB foreground preparation and independent 16 KiB ownership, but acquired
eight stripes at a time to map contiguous unprepared runs together. It passed
25 signed hypervisor tests, including live holes and a partial tail. In three
interleaved cold starts per version, preparation completion fell from 1,576 to
1,455 ms (−7.7%) while Docker readiness rose from 855 to 900 ms (+5.2%). The
extra batching and locking complexity was rejected; the runtime retains the
original 256 KiB preparation path.

The [lowest-background-QoS trial](records/0.5.1/hybrid/m1-background-qos-trial/)
also failed to help. Three interleaved observations each measured Docker-ready
medians of 812 ms for ordinary hybrid, 663 ms for pure demand and 882 ms for
background-QoS hybrid. Preparation completion increased from 1,607 to 7,546 ms.
This policy was removed. Both versions passed the 24 signed hypervisor tests.

The selected candidate retains ordinary-priority hybrid preparation with the
cached task port (`b78dfeb` runtime). The remaining 148 ms startup gap to pure
demand is an explicit trade-off for completing preparation shortly after boot
and restoring whole-range reclamation. The +10% investigation threshold was
exceeded; these experiments document the investigation, not a passing result
against that threshold. Full qualification must not claim pure-demand startup
times for this hybrid, or describe the remaining contention as eliminated.

### Selected-runtime memory and idle validation

[M1 hardware records](records/0.5.1/hybrid/m1-memory-correctness/) retain all
24 passing signed hypervisor tests and the memory/idle gate on the selected
runtime. With 8 GiB configured, a 3 GiB allocation reached a 3,560 MiB physical
footprint, then returned to 267 MiB within five seconds. The next container saw
8,016 MiB MemTotal, consistent with the guest kernel's overhead. Over the
120-second idle window the process used 0.62% CPU, below the 1% gate, and its
final footprint was 327 MiB. The full M1 suite is a separate qualification.

[The remaining ten M1 functional gates](records/0.5.1/hybrid/m1-functional/)
pass from clean source `274293b`, with the unchanged `b78dfeb` runtime. Together
with the separate memory/idle and speed gates, all twelve hardware gates pass.
Coverage includes boot, devices, networking, vsock, Docker, streams, publishing,
filesystem crash recovery, amd64 execution and the daily workflow with clock-skew
recovery. The final CLI, kernel and rootfs hashes match the full-suite inputs.
These are development binaries signed with the hypervisor entitlement; final
notarized-archive qualification remains outstanding.

### Invalid first full-suite attempt

The [first M1 attempt](records/0.5.1/hybrid/m1-invalid-full-a1/) failed during
the boot case's untimed Alpine start with Docker exit 125. Earlier share
observations are retained as incomplete evidence, not a qualified full suite.
The harness discarded Docker's error output, so the original cause is unknown.
An exact-environment retry pulled the image and ran the container successfully.
A preliminary diagnostic lacked the original credential-helper PATH and is
labelled separately; its error does not explain the original failure.

The failure also exposed a cleanup bug: the private boot VM could be left
running before the harness assigned its measurement PID. It was stopped.
`16a7514` preserves start/container diagnostics, stops that exact private home
on failure and retains partial records even when the stage fails. The
[harness checks](records/0.5.1/hybrid/m1-boot-harness-checks/) inject Docker exit
125 and verify error retention and VM shutdown, then run a successful boot-only
case. These are correctness checks, not performance measurements. The existing
benchmark-result tests also pass with a new regression test for this path.

A fresh full-suite attempt uses the same `b78dfeb` VM runtime and the corrected
harness. The already-passed hypervisor and memory/idle checks are not repeated.

### Completed M1 suite

The [first valid full suite](records/0.5.1/hybrid/m1-full/) passes at harness
`16a7514`, runtime `b78dfeb`, with 8 vCPUs and 4 GiB guest RAM. There is one
share stage, one guest-disk stage and one amd64 stage, with three observations
per timed case. The archived summary retains every observation, medians and
sample CVs; these CVs describe repetitions within this suite, not a universal
bound on run-to-run variation. All stage guards and frozen-artifact checks pass.

| Workload | Host share | Guest disk |
|---|---:|---:|
| npm install | 12,849 ms | 7,566 ms |
| pnpm install | 7,561 ms | 1,680 ms |
| yarn install | 10,969 ms | 7,816 ms |
| Tree copy | 7,593 ms | 3,905 ms |
| Tree deletion | 2,827 ms | 613 ms |

The [speed gate](records/0.5.1/hybrid/m1-speed-gate/) passes using this share
stage and the existing September 8 native reference, without new measurements.
Its 85%-of-native npm aspiration remains unmet; the established 30% floor passes.
Guest-disk package medians are close to the historical 0.5.0 primary record;
host-share npm and pnpm are respectively 14.4% and 13.2% slower than that
historical record. The matched comparisons below do not reproduce a consistent
package-install regression, so these historical differences cannot be attributed
to the runtime from this evidence.

The full suite's cold-start medians are 720 ms to Docker and 905 ms to the first
container. These use the Node-based harness at 4 GiB, so they must not be pooled
with the Python-based 16 GiB experiments. Idle footprint is 241 MiB; package-load
footprint peaks at 4,109 MiB and settles to 817 MiB after 60 seconds. As elsewhere,
configured RAM limits the guest, not every host allocation. Idle CPU is 5 ms/s,
approximately 0.5% of one core. The paused Apple processes were restored after
the recorder exited.

### Matched package-cache context

The original isolated screening warmed only the selected package manager. The
full suite warms npm, pnpm and yarn before its first measurement. A
[matched one-observation comparison](records/0.5.1/hybrid/m1-full-warm/) uses
that combined preparation on both versions, with hybrid followed by 0.5.0.

| Workload | 0.5.0 | Hybrid | Time change |
|---|---:|---:|---:|
| npm install | 11,214 ms | 12,160 ms | +8.4% |
| pnpm install | 7,398 ms | 7,402 ms | +0.05% |
| yarn install | 11,804 ms | 11,554 ms | −2.1% |
| Tree copy | 16,858 ms | 17,130 ms | +1.6% |
| Tree deletion | 2,851 ms | 2,861 ms | +0.4% |

Only npm crossed the +5% investigation threshold, triggering two additional
observations per version. The harness can now retain the combined warm-up
through `BENCH_EXTRA_WARM_CASES` while timing npm alone; regression tests verify
that this does not add measurements of the warm-up-only cases. These copy
observations lack the full suite's preceding file-search reads and are not
directly comparable with its warmer copy medians.


The [npm follow-up](records/0.5.1/hybrid/m1-npm-follow-up/) reverses version
order (0.5.0 then hybrid) and retains the same combined package warm-up. It
measures only npm, twice per version; it does not repeat the full suite.

| Version | Initial observation | Follow-up observations | Combined median |
|---|---:|---:|---:|
| 0.5.0 | 11,214 ms | 12,077 / 11,770 ms | 11,770 ms |
| Hybrid | 12,160 ms | 11,615 / 11,493 ms | 11,615 ms |

The combined median changes by −1.3%, while the mean changes by +0.6%.
The ranges overlap, and the initial slowdown does not reproduce consistently.
This small diagnostic sample supports neither a speedup claim nor statistical
equivalence. Each version has an initial observation in one VM and two follow-up
repetitions in another; these are not three independent VM starts. Both follow-up
guards and artifact checks pass, and the paused host processes were restored.
