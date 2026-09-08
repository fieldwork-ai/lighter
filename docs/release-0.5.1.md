# 0.5.1 release qualification

Runtime `b78dfeb3063e3ad7af0660050a766032c36f23c0` is qualified for release.
The [design investigation](demand-memory-2026-09-08.md) explains hybrid RAM
preparation, correctness constraints and the accepted startup trade-off.
The kernel remains **6.18.49** and data epoch **1**.

## Performance qualification

Both hosts completed one full suite with three repetitions per timed case.
The [performance report](../benchmarks/RELEASE-0.5.1.md) is authoritative for
measurements, configuration, methodology, selection/exclusions, variability,
focused follow-ups and fseventsd observations. It also links to the raw inputs.
The selected full suites and speed gates pass; remaining interpretation limits
are recorded there rather than repeated here.

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

The earlier quarter-RAM qualification and artifacts are described
[separately](release-0.5.1-quarter.md); they are not this release's proof.

## Final signed artifacts

The [artifact record](release-0.5.1-artifacts.json) is authoritative for build
and runtime revisions, archive/bootstrap hashes, guest hashes and Apple's
accepted notarization. The frozen archive and bootstrap are the only release
assets. The superseded quarter-RAM candidate must not be published.

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
remaining byte differs. The initial rejection was caused by this signing metadata difference.

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

Launch Services cleanup returned -10814 for some private test bundles;
subsequent registry/filesystem audits found no remaining registrations or homes.

Private signed successor fixtures exercise the updater and rollback. They are
never release assets. Raw diagnostics stay local; Git retains this qualification
summary and the release artifact manifest. A full-day daily-driver soak has
not been performed.

## Review and publication

Qualification is complete. The [publication handoff](handoff-0.5.1.md) owns
the remaining merge/publication sequence and current host/credential state.
