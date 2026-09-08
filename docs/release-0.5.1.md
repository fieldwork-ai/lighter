# 0.5.1 release qualification

The selected runtime is `b78dfeb3063e3ad7af0660050a766032c36f23c0`.
Hybrid RAM preparation moves the creation of host backing objects off the
startup critical path. One ordinary-priority worker completes preparation
while Linux boots; guest CPUs and devices can safely prepare memory ahead
of it. Docker readiness does not wait for the worker.

Independent 16 KiB Mach objects retain the memory-accounting fix. Preparation
uses 256 KiB batches. Once a region is ready, memory accesses bypass preparation
checks and reclamation returns to whole-range calls. Completion publication,
concurrent reclamation and shutdown are synchronized. The cached Mach task
port removes an unnecessary kernel trap for each backing object.

The configured capacity is available to the guest without eagerly making all
of that RAM resident on the host. A VM's RAM setting does not cap every host
process allocation. The guest memory-online gate still applies to restored
containers. The kernel remains **6.18.49**, and data epoch remains **1**.

## Startup trade-off

The [investigation](demand-memory-2026-09-08.md) retains the rejected pure-demand,
priority and mapping-batch alternatives. On M1 at 16 GiB, the selected hybrid
measured 812 ms to Docker versus 663 ms for pure demand, with three interleaved
cold starts per mode. The remaining 148 ms is an accepted trade-off for finishing
preparation shortly after boot and restoring efficient reclamation. The +10%
investigation threshold was exceeded; it was investigated, not passed. These
Python-based experiments are separate from the Node-based full-suite timings.

## Performance qualification

M5 cold-start medians are **517 ms to Docker / 664 ms to the first container**,
versus the historical 0.5.0 primary's 1,649 / 1,799 ms. M1 medians are
**720 / 905 ms**, versus 846 / 1,030 ms. The historical changes are not a
simultaneous experiment. M5 idle memory is 372 MiB; its package-load footprint
peaks at 3,841 MiB and settles to 726 MiB after 60 seconds.

Each host contributes **one complete suite with three repetitions per timed
case**. Memory and idle-power rows are single observation windows. M5 uses
8 vCPUs / 16 GiB RAM; M1 uses 8 vCPUs / 4 GiB. Both use a 128 GiB sparse disk,
the same pinned package fixture and recorded tools/images. Six quiet preflight
observations must stay at or below 5% aggregate host CPU. A competing VM fails
the guard. All selected stages have complete repetitions, successful guards
and unchanged source/artifact hashes.

The [performance report](../benchmarks/RELEASE-0.5.1.md) retains every selected
observation, medians, within-suite CV and historical 0.5.0 differences. The
README leads with M5 and identifies the competitor/native figures as retained
0.5.0 qualification measurements. Historical differences alone do not establish
runtime effects. Focused M1 follow-ups did not reproduce a consistent package
installation slowdown.

The M5 focused copy slowdown did not reproduce. The retained deletion sample
measured 3,015 versus 2,830 ms (+6.5% by median; +8.1% by mean). Its version
effect remains inconclusive: the unchanged hybrid's full-suite deletions span
2,642–3,662 ms with 16.1% within-suite CV. Host/cache/order variation plausibly
contributes; a small runtime effect has not been isolated or excluded. This
is not labelled an established regression or an accepted performance cost.
The +5% threshold triggered a one-case follow-up, not another full suite.
Later informal user-directed repetitions are outside the retained comparison.

Earlier M5 full attempts stopped at guest preflight when the daily VM restarted.
Their completed share stage is retained but excluded from the fresh selected
suite. The user's later shared-host copy diagnostics are also excluded. The
original M1 attempt failed during Docker warm-up; its cause remains unknown
because the inherited harness discarded that stderr. The corrected harness
retains startup failures and cleans up its private VM even before startup
returns a PID. A successful retry and injected-failure checks are recorded.

Across the M5 full suite, all 211 fseventsd samples track one process at
15 MiB, ending at 0.3% CPU. M1's full suite tracks one process at 13–15 MiB,
ending at 14 MiB and 0.3% CPU. Neither required a daemon reset. Five-second
sampling can miss shorter peaks; these observations do not establish a fix
for the earlier unreproduced fseventsd incident.

## Runtime checks

- 343 workspace tests and 24 signed hypervisor tests pass on the selected
  runtime. Formatting, clippy and the CI helper checks pass.
- All twelve M1 hardware gates pass: boot, devices, network, vsock, Docker,
  streams, published ports, filesystem durability, speed floors, memory,
  amd64 and the daily-workload fixture. The full-suite share stage supplies
  the speed-gate measurements; it is not benchmarked again.
- M5 hybrid checks cover concurrent preparation, data preservation, reclamation
  during preparation, region completion, whole-range reclamation, physical
  release/recharge and shutdown. Fresh and restored container capacity matches
  eager mode within 4 KiB at 8, 12, 16 and 32 GiB. Those capacity checks precede
  the cached-task-port change; selected-runtime signed tests also pass.
- M1's selected-runtime memory gate releases a 3 GiB workload from a 3,560 MiB
  peak to 267 MiB within five seconds, preserving full guest capacity. Its
  120-second idle sample uses 0.62% of one CPU core.

The selected M5 memory gate also passes: 3,577 MiB at the 3 GiB workload peak,
290 MiB within five seconds, full configured capacity in the next container,
and 0.26% of one core over the 120-second idle window (1% limit). This was a
shared-host correctness check with the original gate thresholds, without a
quiet-host preflight; it is not an additional performance suite. An earlier
wrapper was cancelled during quiet preflight before any memory measurements.

[Runtime records](records/0.5.1/hybrid/) retain the exact revision and scope of
each check. The earlier quarter-RAM qualification and artifacts are preserved
[separately](release-0.5.1-quarter.md); they are not this release's proof.

## Final signed artifacts

Built from `11e7ec0f2fd1f3d442f7989027ddef8099b9d36e` with runtime `b78dfeb`.
Apple notarization `874a5d1a-fdd5-4ee8-80ac-54bb2e6aa5fa` is **Accepted**
(Developer ID team `N7N6BNF95K`). The [artifact record](release-0.5.1-artifacts.json)
contains the archive/bootstrap SHA-256 values and guest hashes. The frozen
archive and bootstrap are the only release assets; the earlier quarter-RAM
candidate is superseded.

The archive uses portable regular ustar entries. Packaging exercises the real
CLI installer before accepting its output. Independent checks validate the
Developer ID signatures, Gatekeeper assessment, stapled ticket, sealed manifest
and bootstrap equality. Removing signatures from temporary copies and normalizing the derived
`__LINKEDIT` mapping-size field yields byte-identical files to the qualified CLI; the kernel and rootfs
hashes also match. Documentation and formula commits after the build do not
change the frozen release payload.

The first signature-removal-only comparison rejected one byte: signing had
expanded the read-only `__LINKEDIT` mapping across a 16 KiB boundary. The
corrected verifier checks that the original mapping size is exactly the
rounded signed file size before normalizing that derived field. No other
remaining byte differs. [Package evidence](records/0.5.1/hybrid/signed-package/)
retains both the initial rejection and the successful verifier and results.

Both M5 and M1 passed the same six serial checks on the exact signed archive:
fresh installation; migration from 0.5.0; migration from 0.4.1; staged activation
and failed-boot rollback; the development updater lifecycle; and two-node kind.
Kind covers local image loading, cross-node traffic, DNS, TCP/UDP NodePorts,
Mac shares, Helm, persistent volumes, VM restart and ordinary Docker afterward.
Input hashes and the existing daily configuration are unchanged after testing.

M1 Homebrew reinstalled the final archive over its earlier unpublished 0.5.1
candidate, using a local file URL. Repeated post-install, signed payload hashes,
ownership, managed-update refusal, update check, `brew test`, signatures and
Gatekeeper pass. The tap formula was restored clean. This was a reinstall,
not a 0.5.0 Homebrew upgrade; direct signed migrations cover the latter version
transition. The public download check follows publication.

[Signed M5](records/0.5.1/hybrid/m5-signed-archive/),
[signed M1](records/0.5.1/hybrid/m1-signed-archive/) and
[Homebrew](records/0.5.1/hybrid/homebrew-final/) records retain the results.
Launch Services cleanup returned -10814 for some private test bundles;
subsequent registry/filesystem audits found no remaining registrations or homes.

Private signed successor fixtures exercise the updater and rollback. They are
never release assets. Final evidence includes input hashes, commands/results,
migration state checks and cleanup records. A full-day daily-driver soak has
not been performed.

## Review and publication

The [main PR](https://github.com/fieldwork-ai/lighter/pull/5) and
[tap PR](https://github.com/fieldwork-ai/homebrew-tap/pull/5) are ready for review.
The draft `v0.5.1` release contains exactly the final archive and bootstrap;
authenticated downloads match their qualified SHA-256 values. It remains
unpublished. After main merges, point the draft at the reviewed merged commit,
publish those frozen assets, verify both public URLs/hashes, then merge the tap.
The [handoff](handoff-0.5.1.md) gives the sequence. Do not rebuild during publication.

The M5 daily installation now uses the final signed 0.5.1 build, stopped,
with its 16 GiB configuration and data disk preserved. M1's Homebrew 0.5.1 is
also stopped. Neither host has an active lighter or Colima VM. Paused Apple
processes were resumed, the M5 login-service enabled setting was restored,
and the task-owned cached signing credentials were removed. Packaging's
temporary keychain was removed and the normal user keychain list restored.
