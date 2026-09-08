# 0.5.1 release qualification

Status: built, signed, notarized and qualified candidate, ready for dev → main
review. Stable publication follows merge; the final artifact bytes are frozen.

Lighter now starts Linux from its existing quarter-RAM base while a worker
prepares the remaining backing in 128 MiB steps. Independent 16 KiB owned
objects preserve memory accounting and partial reclamation. The change overlaps
work with boot; it does not eliminate the allocation work or promise that the
first container finishes when Docker first answers.

Runtime source is `67d787b5910e0dd68b7564d4716fe3b7b0126d1d`. The signed executable
was built at `dc94d5abf3f4537a12619654eae4322bbcf39151`, whose additional change
is README text. Packaging was corrected at
`e5452e24e19c5c454702531bd18ce4331d64da42`. Removing signatures from temporary
copies produces identical qualified and release executable bytes; kernel and
rootfs hashes also match the qualified inputs. Later documentation, formula and qualification-harness changes do not change
the release payload.

## Startup and memory boundaries

The [boot investigation](boot-preparation-2026-09-08.md) records the design,
rejected alternatives, prototype measurements and observed run-to-run
variation. Its prototype figures are separate from final release measurements.

New container operations wait for complete host backing before the existing
bounded Linux plug wait. Request inspection handles markers split across socket
reads. When saved containers exist, guest init waits for the configured memory
blocks before starting Docker, covering internally restored containers. Saved
stopped containers also take this conservative path. Host pressure can still
reduce usable memory, and total macOS process overhead is not bounded solely
by the guest RAM setting.

On the frozen runtime, M5 startup checks at 8, 12, 16 and 32 GiB compare eager
preparation, fragmented Docker requests and restored containers. Guest
`MemTotal` matches within 4 KiB. Signed hypervisor tests cover unprepared-access
rejection, concurrent preparation without overwriting live pages, and physical
release/reuse under both preparation modes. Three prototype shutdowns during
32 GiB preparation reached guest SystemOff in 442–452 ms; these are distinct
from the early CLI fallback before the guest control service exists.

The kernel remains 6.18.49. The updated root filesystem carries the pre-restore
memory gate and must ship with the VMM.

## Runtime qualification

- Both Macs pass 343 workspace tests and 17 signed hypervisor tests on the
  frozen runtime. Formatting, clippy and the literal CI helper checks pass.
- All twelve M1 hardware gates pass in 563 seconds, covering boot, devices,
  networking, Docker streams and publishing, filesystem durability, speed
  floors, memory reclamation, amd64 and the daily-workload fixture.
- The shared M5 passes 820 concurrent Docker/HTTP checks with zero failures,
  including stdout/stderr, stdin EOF, slow readers and aborted connections.
- A four-run M5 probe shows identical Linux memory-zone geometry with eager
  and background preparation. This does not establish identical allocation
  timing within those zones.

## Performance record

The first valid M1 full suite is attempt 2, selected by execution order.
Attempt 1 stopped before measurement because the local checkout lacked the
published v0.5.0 tag. The record retains every measured repetition, exact source
and artifact hashes, quiet-host checks, competing-VM checks, daemon observations
and measured-case stdout/stderr. Warm-up/setup exit statuses are not retained by
the inherited protocol; absence of diagnostic text cannot validate them later.

The first suite observed Docker readiness at 634 ms and first-container
completion at 821 ms, versus 846 and 1,030 ms in the historical 0.5.0 primary
record. This historical comparison mixes version effects with host-state
changes. Three complete suites report Docker readiness at 634/643/652 ms (1.40% CV)
and first-container completion at 821/859/834 ms (2.30% CV). Other workloads
have their own measured variation, recorded in [Repeatability](../benchmarks/REPEATABILITY.md).

The like-for-like signed archive comparison uses 0.5.0 / 0.5.1 / 0.5.1 / 0.5.0
order, five retained cold starts per arm, identical 8-vCPU/4-GiB profiles and
the same Alpine image. Ratios of geometric means of the two arm medians give:

| Initial state | Docker readiness | Change | First container finishes | Change |
|---|---:|---:|---:|---:|
| Image present, no saved containers | 716.5 → 536.3 ms | −25.15% | 900.9 → 714.9 ms | −20.65% |
| Saved stopped container | 754.4 → 581.3 ms | −22.95% | 934.7 → 759.3 ms | −18.76% |

The saved-container profile exercises the pre-Docker memory gate; it does not
measure restoring an application or cluster. The full suite uses a checkout
CLI/development app and Node timestamps; this signed comparison uses the shipped
app and Python monotonic timestamps. Do not pool their absolute measurements.
The [generated performance report](../benchmarks/RELEASE-0.5.1.md) retains every
arm, within-arm CV, complete-suite measurements and analysis instructions.

Several host-share workloads initially looked slower than the historical
record, prompting a matched 0.5.0 / 0.5.1 / 0.5.1 / 0.5.0 comparison. Each version
uses its own payload and a VMM rebuilt with the same compiler. Ratios of the
versions' geometric means of arm medians give npm −0.56%, pnpm −7.14%, Yarn
+0.49%, ripgrep −34.22%, find +3.57%, copy −4.60% and removal +1.51%. The broad
historical install/copy slowdown did not reproduce. Two arms per version and
the workload-specific variation do not establish precise causal effects or
justify dismissing every small increase as noise.

Across all three suites, storage ABBA and a ten-minute observation afterward,
1,349 samples track one `fseventsd` PID without a reset. Sampled footprint starts
at 13 MiB, peaks at 16 MiB and ends at 14 MiB; post-suite CPU has median 0% and
maximum 0.3% of one core. There are no collection errors or invalid CPU samples.
Five-second sampling can miss shorter peaks. This does not establish a fix for
the earlier incident, which was not reproduced.

The shared M5 was used for targeted startup and functional checks. No full M5
benchmark was run for 0.5.1, as requested. The README's complete comparison
remains explicitly the 0.5.0 M5 record. Its daily VM remains on 0.5.0; a full-day
0.5.1 daily-driver soak has not been performed.

## Packaging failure and correction

The first notarized archive passed system-tar extraction, signature checks and
a fresh VM smoke, but failed real installation with a missing-file error.
A standalone probe reproduced this without a VM: the same 0.5.1 bootstrap
installed the published 0.5.0 archive but rejected the candidate.

BSD tar had encoded the sparse root filesystem using PAX sparse metadata.
The Rust extractor wrote its data beneath `GNUSparseFile.0/` instead of the
manifest's `rootfs.ext4` path. The portable archive uses regular `ustar` entries.
All signed files, the kernel, rootfs and stapled ticket remain byte-identical;
only their tar representation changed. The original failed archive and test
logs are retained. The packager now checks each notarized archive through the
real CLI installer in private state without starting a VM.

The final archive passes that consumer check and independent signature,
Gatekeeper, staple, manifest and qualified-code equivalence checks. Its
[artifact record](release-0.5.1-artifacts.json) retains both archive hashes and
Apple's accepted notarization identifier.

## Signed archive checks

On both Macs, the final portable archive passes fresh installation, arm64/amd64,
published TCP/UDP, guest IPv6 routing and shutdown. Real migrations from both
0.4.1 and 0.5.0 preserve configuration, named volumes and running containers;
explicit restart is enforced. The 0.5.0 migration's restored container reports
the same 4,084,676 KiB `MemTotal` before and after.

Private signed successors exercise stopped-state preservation, concurrent
updater rejection, interrupted-transaction recovery, tamper rejection before
stopping the old VM, explicit running activation, and failed-boot rollback with
container and volume data. These synthetic fixtures are never published.
The separate development update lifecycle passes. M5 daily configuration and
running identity hashes are unchanged, and the seven recorded temporary test
homes have no remaining Launch Services registrations.

The M1 two-node kind check passes cluster creation, local image loading,
Services/DNS/TCP/UDP, host publishing, Mac shared files, port-forwarding, Helm
install/upgrade/uninstall, PVC persistence across Pod replacement and VM restart,
traffic recovery, ordinary Docker afterward and cluster cleanup. The first
attempt stopped before cluster creation because the harness required 0.5.0;
an explicit expected-version option allows the 0.5.1 retry. Both records remain.

Homebrew upgrade and repeated `post_install` pass against the final archive,
including preserved signed payloads and manifest hashes, installation ownership,
direct-update refusal and `brew test`. The temporarily modified tap formula was
restored. M1 is stopped after qualification with the Homebrew candidate installed.

[Final evidence](records/0.5.1/qualification/) includes failed attempts and
successful retries. The three task-owned signing credential files and the
fixture checkout's credential-directory symlink have been removed; the user's
keychain list is unchanged. No M1 background process was paused by this work.

## Review and publication

The final archive and standalone signed bootstrap will remain immutable while
the dev → main PR is reviewed. Publish both only after that PR merges, verify
the public downloads, then make the matching Homebrew tap PR ready to merge.
No 0.5.1 asset is public at this stage.
