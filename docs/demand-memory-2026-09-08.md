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
