# Demand-backed RAM investigation

Status: opt-in prototype, not the signed 0.5.1 release candidate. Publication
is on hold while this approach is qualified. The existing notarized archives
contain the earlier background-preparation implementation.

`LIGHTER_DEMAND_RAM=1` offers the virtio-mem range immediately, preparing backing
on its first CPU or device access. Adding `LIGHTER_DEMAND_BASE=1` applies this
to the initial RAM region too. Defaults are unchanged during qualification.

Each 64 KiB preparation batch contains four independent 16 KiB owned Mach
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
Local raw records and artifact hashes: `.logs/051/demand/m1-latency-a1` and
`m1-latency-a3`; summaries: `analysis-a1` and `analysis-a3`.

## Correctness and remaining qualification

M5 tests use isolated homes while the user's daily VM remains running. Full
demand backing passes new/restored-container MemTotal checks at 8, 12, 16 and
32 GiB (within 4 KiB of eager backing), a twice-touched and verified 2 GiB working
set, volume persistence across VM restart, amd64 execution and the two-node
kind suite including restart/persistence. An earlier hotplug-only variant
also passed 820 Docker/HTTP stream checks. A first 16 GiB attempt failed the
disk-space preflight before starting a VM; its logs are retained separately.

343 workspace tests and 22 signed hardware tests pass. Hardware coverage includes
sparse device access, four simultaneous guest CPUs, instruction retry, partial
reclamation, reuse without duplicate accounting, and injected allocation/map
failures that must abort the entire disposable VM process.

Before default promotion, measure ordinary workloads and reclamation on M1:
first-access allocation moves some work from startup into later execution.
The current release path unmaps/remaps each prepared 64 KiB chunk separately;
its cost also needs measurement. No full M5 benchmark is authorized for this
investigation. A changed release runtime requires fresh qualification,
signing, notarization and package checks.
