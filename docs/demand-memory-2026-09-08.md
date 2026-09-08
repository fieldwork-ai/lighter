# Demand-backed RAM investigation

Status: selected as the default for the new 0.5.1 candidate; frozen-source
release qualification is next. Publication remains on hold. The existing
notarized archives contain the earlier background-preparation implementation
and do not qualify this code.

The default offers the virtio-mem range immediately and prepares both it and
the initial region on first CPU or device access. `LIGHTER_DEMAND_RAM=0` selects
the background comparison path; `LIGHTER_BACKGROUND_RAM=0` with no explicit
demand override preserves the original eager opt-out. `LIGHTER_DEMAND_BASE=0`
keeps the initial quarter eager when demand backing is enabled. The recorder
sets every comparison flag explicitly so future defaults cannot change an arm.

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

M5 tests use isolated homes while the user's daily VM remains running. Full
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
A bounded 64/256/64 KiB comparison then measured the first 2 GiB fill at
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
