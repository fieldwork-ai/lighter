# 0.5.0 qualification

Status: implementation and qualification in progress. The unpublished 0.4.2
candidate is superseded; 0.4.1 remains the public release. Neither release PR
is ready to merge yet.

## Runtime changes

The host now drains queued and in-flight vsock response bytes before
acknowledging a guest's graceful full shutdown. Guest receive shutdown stops
host input independently; it no longer discards response bytes. Explicit resets
and failed writes still abort, returning guest buffers only after their writer
releases them. Both the Docker socket proxy and published TCP reactor honor
these rules. This fixes ordinary Docker output loss discovered during kind
qualification, not a kind-specific retry workaround.

Linux remains 6.18.49 with `CONFIG_NETFILTER_XT_MATCH_STATISTIC=y`, required by
kind's default iptables service rules. The rebuilt agent identifies as 0.5.0.
Installation ownership and verified, explicitly activated updates from the
unpublished 0.4.2 candidate are included.

## Initial evidence

- M5: 10,000 short Docker exec probes, zero failures; 800 concurrent Docker/HTTP
  response checks, zero failures. These first probes used the fixed host and
  the previous guest payload, and are preliminary evidence only.
- 336 workspace tests and Clippy pass, including regressions for queued and
  in-flight response drainage, separate half-closes and reset buffer ownership.
- M5, four vCPUs and 4096 MiB: kind v0.33.0 with pinned Kubernetes v1.35.8,
  one node and default iptables networking passes creation, readiness, local
  image loading, Service and direct Pod traffic, DNS/HTTPS, host TCP/UDP,
  Mac share reads and flushed writes, Helm install/upgrade/uninstall, PVC
  persistence across Pod replacement and VM restart, traffic recovery, ordinary
  Docker operation and cluster deletion. [Results and inputs](records/0.5.0/initial-m5-kind/).

The initial harness failed an immediate Service check after Pod readiness;
Service controller propagation is now given a bounded, logged convergence
window, followed by an independent traffic check. A second harness attempt
read a guest-written file from macOS before flushing it. The durability check
now explicitly fsyncs before inspecting the Mac. Both failed attempts remain
in private raw logs and are excluded from the passing record. These are
functional tests, not benchmark results.

## Expanded functional qualification

All seven kind configurations pass on each host: one and two nodes with
Kubernetes 1.35.8, 1.36.4 and 1.37.0 using iptables, plus two nodes on 1.37.0
using nftables. These cover traffic, DNS, local images, Mac shares, Helm,
persistent volumes, VM restart and deletion. Selected inputs and results are
in [M1 records](records/0.5.0/kind-m1/) and
[M5 records](records/0.5.0/kind-m5/).

Mixed Kubernetes, ordinary Docker and Docker-build memory pressure passes on
the M5 at 8, 12 and 16 GiB. Observed physical-footprint peaks were 8082, 12010
and 15549 MiB respectively; 74%, 71% and 76% of the workload growth returned
within the recovery window. These are sampled workload results, not a
universal guarantee that host overhead cannot exceed the guest configuration.
[Inputs, samples and assertions](records/0.5.0/memory/).

**Additional runtime fix awaiting final qualification:** expanded concurrent
stream testing found intermittent stdin hangs on both hosts (3/820 M1 checks
and 1/820 M5 checks). Instrumentation and standalone C socket-pair tests isolate
a macOS blocking-read EOF race. The host now receives with `MSG_DONTWAIT` and
waits for readiness only on EAGAIN, without periodic timers. It also shuts both
socket directions separately on abort because Darwin may skip write shutdown
when `SHUT_RDWR` finds receive already closed. The readiness C probe passes
100,000 exchanges per host; the diagnostic Lighter build passes 1000 mixed
stdin commands on M1. [Diagnosis and raw reproducer evidence](records/0.5.0/socket-eof/).
Final stress, kind, memory and hardware qualification must be repeated against
the resulting runtime before release benchmarks.

The update lifecycle suite also passes with the 0.5.0 development build:
pending downloads remain inactive, explicit activation preserves data and
configuration, and automatic-download preferences persist.

## Remaining release gates

1. Qualify one and two nodes on M1 and M5 with pinned Kubernetes 1.35.8, 1.36.4
   and 1.37.0; qualify nftables separately. Repeat output, slow-reader and abort
   stress on both hosts, plus mixed Docker/Kubernetes pressure at 8/12/16 GiB.
2. Freeze the runtime and guest hashes; run full CI, signed hypervisor tests and
   all hardware gates against them.
3. Complete full benchmarks on both hosts, including three consecutive suites,
   ten-minute fseventsd observation, five fresh same-build storage runs and
   alternating 0.4.1/new/new/0.4.1 storage comparisons. Enforce quiet-host and
   competing-VM checks continuously; retain and exclude invalid runs. Refresh
   native/installed competitor baselines and regenerate README from raw CSVs.
4. Build, sign, notarize, staple and test exact release artifacts on both hosts.
   Exercise published 0.4.1 migration, private 0.4.2 migration, explicit update
   activation, rollback and Homebrew ownership. Future fixtures stay private.
5. Make dev → main ready with immutable artifacts and evidence. Main moves only
   through review. Publish after merge, verify public assets, then make the tap
   PR ready. Clean task-owned signing credentials and restore host settings.

The M5 daily VM and Colima remain stopped. The daily configuration stays at
16 GiB; release installation must preserve its stopped state. Private test VMs
are separate. Never restart fseventsd to improve a measurement.
