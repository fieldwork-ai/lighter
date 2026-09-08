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
