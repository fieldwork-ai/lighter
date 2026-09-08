# Preparing RAM alongside Linux boot

Status: background preparation was qualified in an unpublished 0.5.1 candidate.
Publication is on hold for the newer [demand-backed RAM investigation](demand-memory-2026-09-08.md).
The initial measurements below are prototypes, not final signed-release results.

The accounting fix introduced in 0.4.1 gives each 16 KiB host page its own
nonvolatile owned memory object. This prevents duplicate charging when both
macOS and the guest access it and preserves physical release of individual
pages. A 16 GiB VM requires 1,048,576 objects. Previously, all of those objects
and their guest mappings were prepared before Linux could execute.

We retain those independent objects. Linux starts from the existing quarter-RAM
base (minimum 1 GiB), while one background thread prepares the remaining range
in 128 MiB steps. A step is offered through virtio-mem only after its backing
and guest mapping are complete. This overlaps preparation with Linux and
service startup; it does not eliminate the remaining allocation work.

## Measurements and variation

Raw repetitions and artifact fingerprints
compare the same prototype executable and guest image with background
preparation enabled and disabled. Each memory size uses ABBA followed by BAAB,
three cold starts per arm, with one untimed preparation start per arm. Images
are present locally before timing. The combined arm figures below use geometric
means of the two arm medians, separately for each sequence.

| Host / guest RAM | Docker readiness, original → background | First container, original → background | Relative time reduction, readiness / first container |
|---|---|---|---|
| M1 / 4 GiB, ABBA | 799 → 635 ms | 984 → 815 ms | 20.6% / 17.2% |
| M1 / 4 GiB, BAAB | 811 → 633 ms | 987 → 813 ms | 22.0% / 17.6% |
| M1 / 2 GiB, ABBA | 651 → 610 ms | 835 → 790 ms | 6.4% / 5.5% |
| M1 / 2 GiB, BAAB | 658 → 620 ms | 832 → 802 ms | 5.7% / 3.6% |
| Shared M5 / 16 GiB, ABBA | 1490 → 682 ms | 1620 → 1230 ms | 54.2% / 24.1% |
| Shared M5 / 16 GiB, BAAB | 1637 → 705 ms | 1816 → 1296 ms | 56.9% / 28.6% |
| Shared M5 / 12 GiB, ABBA | 1425 → 663 ms | 1603 → 1079 ms | 53.5% / 32.7% |
| Shared M5 / 12 GiB, BAAB | 1744 → 714 ms | 1920 → 1198 ms | 59.1% / 37.6% |

The M5 remained shared, including its daily VM. Its 12 GiB original-path runs
were slower than earlier 16 GiB runs, demonstrating substantial host drift.
These results support the direction of the improvement, not a precise M5
release claim. No full M5 benchmark was run for this investigation.

Within each identical three-repetition arm, CV is sample standard deviation
divided by mean. On the M1, median within-arm CV ranges from 1.2–3.2%, with a
maximum of 4.0%. On the M5, the median ranges from 2.5–5.3%, but one background
16 GiB arm reaches 26.5% for readiness and 18.8% for first-container completion.
Short-term variation and drift between arms are different; pooling all M5
samples into a single noise threshold would hide that distinction. These
small samples do not establish a universal variance bound.

Readiness is polled with `docker version`, with 50 ms between failed commands;
command execution adds to that sampling interval. First-container time runs
from CLI invocation through completion of an already-present Alpine image.
It includes any memory preparation the first container must wait for. Neither
number is the isolated Linux kernel boot time.

The saved-container comparison is retained separately. It waits for all RAM
before Docker starts. Its M5 original arm medians drifted from 1.59 to 3.71 s;
we do not use its relative figures as a release speed claim.

## Correctness boundaries

- Unprepared addresses remain unavailable to the guest and host device models.
  Publication follows successful creation and mapping; live prefixes are never
  overwritten. Tests cover concurrent preparation, page release and reuse.
- Available memory grows monotonically. A pressure reduction changes the
  requested target independently; later preparation does not overwrite it.
  The advertised usable prefix and requested size follow the
  [virtio-mem configuration rules](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html).
- New Docker container operations wait for complete host backing before the
  existing guest plug wait begins. A backing timeout or shutdown closes the
  request without forwarding its remaining bytes. Connection-local overlap
  catches request markers split across socket reads.
- Docker restores containers internally. The first prototype let those
  containers see roughly 9.5–9.9 GiB in a configured 16 GiB VM. Guest init now
  waits for all configured memory blocks to be online before starting Docker
  whenever saved container metadata exists. This includes stopped containers;
  the implementation does not parse Docker's private restart-policy schema.
- Functional checks at 8, 12, 16 and 32 GiB compare startup `MemTotal` with the
  original path. Fragmented API requests and restored containers match within
  4 KiB. Three 32 GiB stops during preparation complete through guest SystemOff
  in 442–452 ms. An earlier stop before the guest control service exists uses
  the CLI's existing immediate-exit fallback; that is not a graceful guest
  shutdown test.

The existing 16 KiB accounting/reclamation tests pass with background backing
on both Macs. This preserves accounting semantics; it does not turn the
configured guest RAM into a hard cap on all host process overhead.

`LIGHTER_BACKGROUND_RAM=0` retains the original preparation path for comparison.
`LIGHTER_BOOT_TIMING=1` writes phase spans with monotonic timestamps to the
machine log and CLI stderr. Neither is needed for normal operation. The updated
root filesystem is part of the release: its pre-restore memory gate must travel
with the VMM.

## Final release qualification

The frozen runtime and final signed, notarized archive have completed
qualification. [The release report](release-0.5.1.md) records all three full M1
suites, separate comparisons of the signed release archives, installation,
rollback, kind and Homebrew checks. The final image-only signed comparison
measures Docker readiness at 716.5 → 536.3 ms and first-container completion at
900.9 → 714.9 ms on M1. These are distinct from the prototypes above.

Existing README benchmark tables remain identified as 0.5.0 M5 measurements.
The shared M5 was not used for a full replacement record. Publication follows
the dev → main release PR merge.
