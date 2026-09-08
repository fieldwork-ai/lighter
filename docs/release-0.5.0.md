# 0.5.0 qualification

Lighter 0.5.0 adds qualified local Kubernetes through kind, fixes Docker socket
shutdown handling and includes verified explicit upgrades. The unpublished
0.4.2 candidate is superseded; its artifacts remain private migration inputs.

## Runtime and guest

Graceful vsock shutdown now drains queued and writer-held response bytes before
resetting the connection. Guest receive shutdown stops host input independently.
Abort still returns buffers only after the writer releases them. This prevents
short Docker commands from succeeding with truncated or empty output.

Concurrent stdin testing also reproduced a macOS blocking-read EOF race outside
Lighter in a C socket-pair test. Host reads now use `MSG_DONTWAIT` and wait for
readiness on EAGAIN, without periodic polling timers. Abort shuts each socket
direction separately because Darwin can skip write shutdown after receive has
already closed. Diagnosis and standalone reproducer.

Linux remains 6.18.49, rebuilt with `CONFIG_NETFILTER_XT_MATCH_STATISTIC=y` for
kind's default iptables Service rules. The guest agent identifies as 0.5.0.
Installation ownership and explicit verified updates include the CLI, VM,
kernel and root filesystem as one release; see [updates](updates.md).

Runtime code is frozen at `66897086c3702b75d79cb6fcfed7736842b80780`.
Hardware and benchmark source is `1b0caabf34fbe359f11cd47cc87ce45dc8583682`;
intervening changes concern documentation and test/benchmark tooling. Each
host's binaries are fingerprinted independently, and the guest payload is
identical on both:

- Kernel: `0b4d835c10f6c5849ee85d4ccab53dec8e0dd3de2dcdb55cc128f78d68a034da`.
- Root filesystem: `84cad61484b67dec2875c4bd8a812377e4ce76157dd0c78e5ab14c45d4505b44`.

## Functional evidence

Both hosts pass formatting, Clippy, 339 workspace tests and all 15 signed
hypervisor tests. All twelve hardware gates pass with identical fingerprints
before and after. M1 qualification and
M5 qualification retain logs, measurements
and the corrected build protocol. Hardware-gate timings are not the primary
release performance record.

Each host passes 10,000 short Docker execs, 10,000 mixed stdin commands and
2,050 concurrent stream checks, with zero failures. The stream workload covers
stdout/stderr, stdin EOF, published TCP responses, slow readers and abort.
Raw compressed checks and hashes.

All seven kind configurations pass on each host: one and two nodes using
Kubernetes 1.35.8, 1.36.4 and 1.37.0 with iptables, plus two nodes on 1.37.0
with nftables. Tests use kind v0.33.0, native arm64 nodes and IPv4. Coverage
includes creation/deletion, local images, Service and cross-node traffic,
DNS/HTTPS, host TCP/UDP, Mac shares, Helm, PVCs and VM restart recovery.
M1 inputs and results and
M5 inputs and results use the exact qualified CLI
hash for their host. [The guide](kubernetes.md) explains setup and tested scope;
this is not a Kubernetes conformance claim.

Mixed Kubernetes, ordinary Docker and Docker-build pressure passes at 8, 12
and 16 GiB on M5. Workloads concurrently hold incompressible memory, check live
Service traffic and observe the task footprint for up to 180 seconds afterward. The return
percentage uses the lowest observed recovery footprint against the pre-load
baseline and peak.

| Configured guest RAM | Sampled peak task footprint | Peak-to-baseline footprint growth returned |
|---|---:|---:|
| 8 GiB | 8083.96 MiB | 74.34% |
| 12 GiB | 11995.23 MiB | 82.05% |
| 16 GiB | 15531.63 MiB | 72.87% |

The 0.4.1 accounting correction is retained. These sampled workload results do
not make guest RAM an absolute ceiling on host overhead. They do show peaks
below each configuration in these runs, followed by a reduced task-footprint charge. The separate physical-page
experiments behind the [0.4.1 correction](memory-accounting-2026-09-06.md)
verify the retained release mechanism; task footprint alone does not count
distinct resident pages.
Inputs, samples and assertions.

## Release measurements

Both hosts completed three full Lighter suites, five fresh storage runs and
an alternating 0.4.1/0.5.0/0.5.0/0.4.1 comparison. Reversed-order follow-ups
retain all earlier results. The baseline VMM is rebuilt from v0.4.1 source with
the same Rust toolchain; its guest payload comes from the published archive.
The first complete suite is the primary record for each host. Native tools
and container inputs are pinned, and installed competitor configurations are
recorded explicitly. [Complete measurements and plots](../benchmarks/RELEASE-0.5.0.md),
[workload-specific repeatability](../benchmarks/REPEATABILITY.md) and
[competitor methods and exclusions](../benchmarks/RELEASE-0.5.0-COMPETITORS.md)
explain the selection and protocol limits.

Several apparent changes reverse with test order. M1 Yarn takes 4.84% and 5.12%
longer in the two comparisons; M5 share tree copy takes 7.56% and 4.32% longer.
These are possible costs, with small samples and same-build variation limiting
precision and causal interpretation. M5 pnpm is modestly quicker in both
orders. The release makes no general speedup claim.

Filesystem-daemon observation retains the same PID on each host. During the
post-suite window, median CPU was 0.0% on M1 and 0.1% on M5; final observed
footprints over each full Lighter recording sequence were 4.55 and 12 MiB.
One impossible negative M1 CPU sample is retained and excluded only from CPU
statistics. Five-second sampling and rounded memory values limit precision.
These observations do not establish a fix for the historical unreproduced
fseventsd incident.

The benchmark audit excludes M1 Colima's failed share pnpm case. It also
excludes Docker Desktop share installations with failed cleanup and dependent
storage/package-load memory observations. The original harness ignored those
setup failures; future runs now reject them and retain diagnostics. Raw records
and original statuses remain available. Independent cases and guest-disk
results are retained rather than replacing failed inputs with favorable reruns.

After all competitor measurements and the separate UDP verification, final
quiet observation ended at 5.59 MiB on M1 and 12 MiB on M5, with median CPU
0.0% and 0.1% respectively and unchanged daemon PIDs. All competitor restoration
commands succeeded. The M1 wallpaper and user maintenance processes temporarily
paused for the last Docker Desktop stage were restored after observation.

## Exact release artifacts

The final archive and signed bootstrap were built from
`fe9370566ae082ff2c0d5b3f642354f6ec388de9`, after the complete benchmark record.
Apple accepted notarization submission `1ebc15c8-badd-4c5b-a621-6db2b2c78cbf`.
Staple validation and Gatekeeper assessment pass. Removing signatures only
from temporary copies proves both final executables equal the qualified M5
code; kernel and rootfs hashes also match.

- Archive SHA256: `4da3fca84dbaf6d1ede81d682efbb52df64db438cfeef9679d3e60eb05e8df52`.
- Bootstrap SHA256: `594b09e923358787fed3c67788fa95a6d76f172b8132daae3e55d64982b9438a`.

[Artifact metadata](release-0.5.0-artifacts.json) and
exact-artifact records retain the signed
manifest, verification, stage logs and input hashes. Both Macs pass fresh
smoke, published 0.4.1 migration, private 0.4.2 migration, explicit activation,
failed-boot rollback, and two-node kind with VM restart and persistent data.
M1 exercises the real login-service path; M5 preserves its existing daily
login service. The separate development-layout test confirms pending updates
remain inactive through ordinary restart and background-download preferences.

M1's first preflight found missing private future fixtures; a later kind
wrapper found its private executable absent before cluster creation. Those
attempts are retained. After provisioning, source and every input hash were
checked unchanged before completing the remaining stages; earlier passing
stages were retained. Neither correction changed release code or artifact bytes.

The actual M1 Homebrew upgrade passes, including repeated postinstall,
byte-for-byte CLI comparison, every manifest hash, signature and Gatekeeper
verification, and refusal of direct updates from the Cellar. The test formula
was restored, as was Homebrew's automatically enabled developer setting.
Private future fixtures must never be published.

## Review, publication and cleanup

The release is prepared for [dev → main review](https://github.com/fieldwork-ai/lighter/pull/4).
The [tap PR](https://github.com/fieldwork-ai/homebrew-tap/pull/4) carries the same
archive checksum and stays draft until public assets exist. Publish the archive
and signed bootstrap immediately after main merges, verify public download
hashes, then make the tap PR ready. The new installer requires the standalone
bootstrap absent from 0.4.1. Published bytes must remain immutable.

Both task-owned Colima benchmark profiles were deleted, with existing default
profile checksums unchanged. Competitor settings and container state were
restored, and all test VMs are stopped. Docker context selections and existing
Lighter endpoints were restored. The three task-owned signing cache files were
removed after verification; the original login-keychain search list is restored.

The M5 daily installation remains on public 0.4.1, stopped, with 16 GiB memory.
After publication, install the public release while preserving its configuration,
data and stopped state. The M1 test installation is the verified 0.5.0 Homebrew
candidate. Neither fseventsd process was restarted during this qualification.
