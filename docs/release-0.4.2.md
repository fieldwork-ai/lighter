# 0.4.2 qualification

**Publication hold, 2026-09-07:** subsequent kind qualification reproduced
ordinary Docker commands losing their output during vsock teardown. The release
PR is back in draft. The staged artifacts below remain unchanged, but prior
passing checks do not cover this newly reproduced failure. See the
[kind investigation](kind-qualification-2026-09-07.md) for the deterministic
regression, captured traces and separate Kubernetes kernel limitation.

0.4.2 records installation ownership and adds explicit release updates. Direct
installations can check and download updates, with optional daily background
downloads. Activation requires an explicit upgrade; a running VM requires
`--restart`, and a stopped VM stays stopped. Homebrew retains ownership of its
Cellar files. Linux remains 6.18.49, delivered with the matching guest payload.

## Qualified source

Runtime: `7d2123d0db883b7e8c6d3301ec29e4c17fbe1964`.
The full M1 qualification and benchmark source is
`cc717e79d5e2a53e5fd46d1e0cc0399698daa67a`, which adds only the candidate archive
checksum. Later formula formatting, measurements and release documentation do
not change the Rust runtime or guest payload.

The guest agent was rebuilt with version 0.4.2. There are no filesystem,
memory-policy, kernel or guest-agent behavior changes relative to 0.4.1.
The CLI adds ownership validation, signed-payload verification, separate
background downloads and transactional activation/recovery.

## Completed validation

| Check | Evidence |
|---|---|
| Workspace | 333 tests passed, including 25 CLI tests; formatting and all-target Clippy passed |
| GitHub CI | The PR checks pass; hardware tests remain separate because hosted runners cannot create these guests |
| Hypervisor | 15 signed tests passed on both hardware hosts; the final M1 qualification repeats them on the frozen source |
| M1 hardware | All twelve gates passed on `cc717e79`, including filesystem durability, memory reclamation, published networking and amd64 |
| M5 hardware | All eleven non-performance gates passed on `d4e9f91`; final signed tests below cover the subsequently changed CLI paths |
| Actual installer migration | Published 0.4.1 → candidate 0.4.2 passed on both hosts, including doctor and amd64 smoke; configuration, named-volume data and an automatically restarting container survived |
| Activation and recovery | Private signed future fixtures passed stopped/running activation, concurrent-updater refusal, kernel-tamper rejection before shutdown, interrupted-transaction recovery and failed-boot rollback on both hosts |
| Login service | The M1 repeated the activation/recovery tests through its real user launch agent; stopped upgrades stayed unloaded, explicit start worked, and rollback restored the running container and data |
| Background downloads | A pending update stayed inactive through normal restart and through enabling, polling and disabling the real background agent |
| Homebrew | Actual upgrade and reinstall passed, including repeated postinstall, exact CLI byte comparison, manifest hashes, Gatekeeper assessment and refusal of direct updates from the Cellar |

On M1, optional PostgreSQL client checks and external/global IPv6 probes
were unavailable; the PostgreSQL port and available networking checks passed.
The corresponding M5 functional gates passed.

The future fixtures are private test artifacts, not published releases. One
contains a deliberately invalid kernel authenticated by its test manifest so
that qualification reaches the boot-failure rollback path. No corresponding
future tag or public asset is created.

A full working-day sleep/wake and VPN-change soak has not been performed for
0.4.2. The hardware gates and update lifecycle tests cover different conditions.

Qualification caught and corrected three release-path problems: macOS
AppleDouble archive sidecars, literal-requirement syntax for `codesign`, and
launchd briefly retaining a service after `bootout` returns. Final review added
a selection check under the activation locks, preventing an in-flight updater
from replacing a generation selected by another installer. Failed downloads
also respect the hourly retry backoff even when the preceding metadata check
succeeded.

## Measurements

The first complete M1 suite is the prespecified primary record. Two subsequent
full suites measure repeatability and exercise filesystem-daemon recovery,
without restarting the daemon. Each suite includes host-share, guest-disk and
amd64 stages. A ten-minute observation follows the final suite. The user later made M5 available, but two attempts were invalidated when
other VMs started during measurement; neither supplies a release performance
record. The daily VM was restored with its original 16 GiB configuration.

All nine benchmark stages and the ten-minute daemon observation passed.
[The full report](../benchmarks/RELEASE-0.4.2.md) retains every sample and
workload-specific variation. Across 726 valid observations, the same daemon
PID peaked at 6.09 MiB and ended at 4.72 MiB. Post-suite CPU had a median of
0.0% and maximum of 0.3%. There were no monitoring errors. The fresh-daemon 0.4.1 baseline and the previously published
0.4.1 record are both retained, alongside all new repetitions and outliers.

A subsequent alternating M1 package-install comparison (0.4.1 / 0.4.2 /
0.4.2 / 0.4.1) completed all four arms. Comparing geometric means of the two
arm medians per version gives npm −1.15%, pnpm −2.71% and yarn +2.49%. The
larger apparent slowdown did not reproduce consistently. All samples and
limits are retained in [the follow-up report](../benchmarks/RELEASE-0.4.2-ABBA.md).

A successful soak does not establish a fix for the historical `fseventsd`
incident: its original trigger remains unreproduced. macOS task footprint
includes compressed memory and host allocations; the configured RAM ceiling
limits guest memory rather than all host-process overhead.

## Final packaging and publication

The final package was built from
`eb93bf92c11fd9b7cf48186fa1e24bedb4ee897b`, after the measurement records.
Apple accepted notarization submission
`3f30e8e7-e924-4442-86ef-1c522d93788c`; the app was stapled and both ticket
validation and Gatekeeper assessment passed.

- Archive SHA256: `3f5a02bc8b3b44ae5afb51ee10fed249af1f01c17137d9d483170553c2a966d7`
- Signed bootstrap SHA256: `049dabdd6f8db531bc4b8b33eb0517eadb7dfe98f4e9f8fd1f8ff7e061bd21bc`

The final CLI and app executable, after removing signatures from temporary
copies, are byte-identical to the qualified candidate. Kernel, rootfs and
kernel-version hashes also match. [Artifact verification metadata](release-0.4.2-artifacts.json)
retains those hashes and the packaged source.

The exact final archive passed fresh VM startup, doctor, arm64 and amd64
containers, published TCP/UDP, guest IPv6 route, shutdown and real 0.4.1
migration on M1. Final Homebrew reinstall, repeated postinstall, CLI byte
comparison, manifest verification, Gatekeeper and direct-update refusal pass.
M5 verifies the final signatures, staple and code/payload equivalence; its
previous candidate migration and activation tests cover the identical code.
No further M5 VM test is claimed while competing VM work is active.

The release remains unpublished and the tap PR stays in draft pending the
main PR merge. Merge and publication must be coordinated: publish the prepared archive and signed
bootstrap immediately after merging, since the new installer requires a
bootstrap asset that 0.4.1 did not provide. Verify public download hashes, then
make the matching tap PR ready to merge. Never replace published asset bytes.

The M5 daily VM remains on released 0.4.1 at 16 GiB. All three task-created
signing credential cache files were removed after final verification, and the
original user keychain search list was restored.
