# kind qualification, 2026-09-07

**Not qualified.** An isolated M5 test of the signed 0.4.2 candidate found a
Docker response-loss bug and a missing kernel feature used by kind's default
service networking. The release PR has returned to draft: response loss affects
ordinary Docker commands, independently of Kubernetes. No release bytes or
production runtime source were changed by this investigation.

## Configuration

- Lighter candidate archive SHA256:
  `3f5a02bc8b3b44ae5afb51ee10fed249af1f01c17137d9d483170553c2a966d7`.
- kind v0.33.0, kubectl v1.35.8, native arm64 Kubernetes v1.35.8.
- Node image:
  `kindest/node:v1.35.8@sha256:07b2536e30b803ed61d1677a79df6115f798ce64c80f9e22f6ed45afd09323c0`.
- Four vCPUs, 6144 MiB guest memory, 64 GiB disk, localhost publication.
- Docker client 29.6.1, guest Docker 28.3.3, Linux 6.18.49-lighter.
- Separate machine home, Docker socket/config and kubeconfig. Other VMs and
  daily work continued on the M5. These are functional observations, not
  controlled performance measurements. No test VM was started on the M1.

Exact inputs and selected logs are in the record directory.
The latest kind release offered several node versions; this first qualification
deliberately pins 1.35.8. It does not establish a supported version matrix.

## Blocker: Docker output discarded during socket close

The first `kind create cluster --wait 180s --retain` bootstrapped the control
plane but panicked in `waitforready.go:122`: its readiness command returned no
output, and kind indexed an empty result. The node subsequently became Ready.
This was not a successful cluster-creation test.

A simple `docker exec <node> printf kind-exec-ok` isolated the symptom. The
preserved second release run lost output from **3 of 100** commands, all with
exit status zero. An earlier exploratory run observed 2 of 100, but its raw
file was overwritten; the preserved second run is the primary record. A
diagnostic build with logging only reproduced **2 of 100**. A later run with
iteration-specific tokens passed 100 of 100; that does not clear an intermittent
failure. Running the original 100-command loop directly against the Docker
socket inside the guest printed no failures.

The diagnostic trace provides the mechanism. Two full-close packets for the
Docker vsock port arrived with `received=230`, `fwd_cnt=210`, and `outbound=20`:
the 12-byte command output plus Docker's 8-byte frame header was still queued.
`Vsock::handle(Op::Shutdown)` closes the host socket, removes the connection,
and retires those queued bytes before its writer can deliver them. The trace
also caught undelivered data on the published-network stream port.

The recorded patch adds logging and one regression test to an isolated source
checkout. The test queues response bytes and then delivers a full shutdown
before the writer runs. It deterministically fails: expected `final-output`,
received `None`. The patch is diagnostic evidence, **not a fix**, and the failing
test has not been added to the normal CI suite.

[The executable probe](../scripts/test-exec-output.py) now records every command
and fails on missing output even when Docker returns success. Use an explicitly
selected test engine and an existing container that provides `printf`:

```sh
DOCKER_HOST=unix:///path/to/test/docker.sock \
  python3 scripts/test-exec-output.py --container test-container \
  --reps 100 --output /tmp/exec-results.json
```

The transport fix must drain queued **and already handed-off** bytes before
graceful teardown, preserve guest buffer ownership until writes complete, and
retain an abort path for failed host writes. Both the threaded Docker proxy and
the network reactor consume this transport. A delay or retry in kind would
hide the bug rather than fix it.

## Blocker: default iptables service networking

The default single-node cluster installed its CNI and storage provisioner and
ran Pods, but kube-proxy repeatedly failed `iptables-restore`:

```text
Extension statistic revision 0 not supported, missing kernel module?
```

The guest kernel configuration does not enable
`CONFIG_NETFILTER_XT_MATCH_STATISTIC`. Default service HTTP timed out and the
host NodePort returned an empty response. Early DNS success did not establish
that the complete service ruleset was correct.

The documented `networking.kubeProxyMode: nftables` setting allowed service
traffic without modifying the shipped kernel. That is a useful configuration
to continue testing, not proof of default kind compatibility. Supporting the
default would require enabling the missing kernel feature and qualifying the
resulting kernel, including ordinary Docker behavior.

Sources: [kind configuration](https://kind.sigs.k8s.io/docs/user/configuration/),
[kind v0.33.0](https://github.com/kubernetes-sigs/kind/releases/tag/v0.33.0).

## Functional results around the blockers

The final two-node cluster used the exact release candidate, nftables from
creation, and `--wait 0s` to avoid the crashing readiness action. Readiness and
application reachability were checked separately. Skipping that action is a
diagnostic workaround; it does not make cluster creation reliable while Docker
output can disappear.

| Check | Observation |
|---|---|
| Two nodes | Control plane and worker both Ready |
| Local Docker build | Image built locally and loaded into both nodes; Pods used `imagePullPolicy: Never` |
| Cross-node traffic | Client on the control plane reached the application on the worker through its Service |
| DNS and egress | External DNS and outbound HTTP passed |
| Host access | NodePort through an explicit localhost mapping passed; single-node `kubectl port-forward` passed |
| Mac files | A dedicated share reached the worker Pod; reads and writes crossed the share |
| Persistent storage | PVC data survived Pod replacement in the single-node test and a VM restart in the two-node test |
| Restart recovery | After the exact release VM restarted, cross-node service traffic, host NodePort and stored data recovered |
| Cleanup | Both clusters deleted; no containers remained in the test engine; isolated VM stopped |

An early single-node restart probe tested traffic before networking recovered:
it recorded DNS failures and an HTTP failure despite the Deployment reporting
available. Later service and host requests passed; both observations are kept.
The two-node recovery probe consequently waits for actual end-to-end traffic
with bounded requests, rather than trusting potentially stale availability
status alone. This is startup recovery evidence, not an availability or timing
guarantee. One preparatory command also had a misspelled kubectl flag; it failed
before application creation and was corrected.

No Kubernetes conformance suite, Helm workload, dual-stack cluster, arbitrary
CNI, sustained pressure test, sleep/wake test, or additional Kubernetes version
was qualified. No speed or efficiency comparison against kind on other engines
is justified by this busy-host run.

## Resources and disposition

162 diagnostic samples recorded a maximum test-VM task footprint of 3730.23 MiB.
The same fseventsd PID remained throughout; its maximum sampled RSS was
11.91 MiB. RSS is not Activity Monitor's footprint metric. Other work was active,
sampling did not cover every instant, and these values do not establish an idle
baseline, a memory ceiling, or causal attribution. The monitor's `phase` label
was not advanced; use timestamps and VM PIDs, not that label, to interpret it.

The user's daily 0.4.1 VM retained the same PID, configuration and wrapper.
The private diagnostic build was stopped, the normal checkout build restored,
and monitoring stopped. Full cluster exports and kubeconfigs remain private;
the committed record excludes credentials and omits the external HTTP page body.

Before qualification can pass: fix and stress-test graceful stream teardown,
decide and document the supported proxy mode, then rerun creation, application,
network, storage, restart and cleanup gates on the exact resulting build. The
new Docker bug also requires release requalification before publishing 0.4.2.
