# lighter

Docker for macOS, open-source and headless.

lighter is a virtual machine monitor built directly on `Hypervisor.framework` in Rust. It implements its own vCPU loop, GICv3 interrupt controller and virtio devices, and runs a custom Linux LTS kernel. Use Docker and Compose from the command line, and run local Kubernetes clusters through kind, without a desktop app.

**MIT or Apache 2.0 licensed, your choice. No commercial subscriptions, no paid tiers, no "free during beta", and no telemetry.**

Apple Silicon, macOS 15 (Sequoia) or later.

---

## Why lighter?

lighter controls the VM runtime, shared filesystem and host networking together:

- **Bespoke storage and filesystem:** In-memory cached reads with macOS `FSEvents` invalidation, event-loss recovery and finite cache leases over `lighter-fs`, paired with an internal `btrfs` disk using reflink clones and inline completions. The [benchmarks below](#benchmarks) measure file-change latency and storage work on both a host share and the guest disk.
- **Packetless networking:** No userspace TCP/IP stack or virtual network card overhead. Container sockets are bridged to host sockets over vsock, with BPF sockmaps carrying the guest data path where available.
- **Dynamic memory via virtio-mem:** Sized to what it actually runs. The guest expands for containers and releases unused memory afterward. Nonvolatile owned backing removes duplicate host/guest charges while preserving reclamation.
- **Measured startup:** Cold-start and first-container timings are recorded on both Macs below. Correct memory accounting adds initialization work that scales with the configured RAM ceiling; the [release comparison](benchmarks/RELEASE-0.4.1.md) records that cost.
- **Local Kubernetes through kind:** One- and two-node arm64 clusters are qualified on M1 and M5. Use standard kind, kubectl and Helm commands; the [guide](docs/kubernetes.md) records tested versions and scope.
- **No GUI:** A headless daemon or launchd service. Idle CPU and wakeups are measured alongside the other runtimes below.
- **LTS kernel strategy:** Tracks upstream Linux Longterm Support (LTS) releases with a minimal set of hypervisor-focused patches, updated regularly with upstream point releases.

---

## Install

### The one-line installer

```bash
curl -fsSL https://raw.githubusercontent.com/fieldwork-ai/lighter/main/scripts/install.sh | sh
```

Updates are explicit: `lighter upgrade` applies a verified release, and a
running VM requires `lighter upgrade --restart`. Direct installations can
opt into background downloads with `lighter update auto-download on`;
downloads never activate themselves. Homebrew installations stay managed by
Brew. [Installation ownership, migration and update behaviour](docs/updates.md).

### Or via Homebrew

```bash
brew tap fieldwork-ai/tap
brew install lighter
```

Then start the daemon:

```bash
lighter start
```

`lighter start` boots the VM and registers a Docker CLI context. Your existing `docker` and `docker compose` commands point at it immediately, with nothing to export and no manual socket flags.

```bash
docker run --rm alpine echo "hello from lighter"
```

Useful commands:

```bash
lighter status      # VM state, vCPU count, memory footprint, and disk usage
lighter doctor      # Verify macOS hypervisor entitlements and configuration
lighter config      # View or change CPU, memory, and disk allocations
lighter install     # Register with launchd to start automatically on login
lighter stop        # Cleanly shut down the machine
```

---

## Kubernetes

Run local Kubernetes clusters with kind on Lighter 0.5.0:

```sh
brew install kind kubectl
lighter start
docker context use lighter
KIND_EXPERIMENTAL_PROVIDER=docker kind create cluster --name dev \
  --image kindest/node:v1.37.0@sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5 \
  --wait 180s
kubectl --context kind-dev get nodes
```

Use ordinary kind, kubectl and Helm commands for local development. The
[setup guide and tested scope](docs/kubernetes.md) cover the pinned version
matrix, local images, ports, Mac files, persistent volumes and restart behavior.

---

## Why it is fast

The performance of containers on macOS comes down to five bottlenecks: the shared filesystem, virtual disk I/O, memory management, the network, and the time between asking for a container and having one.

### 1. Shared filesystems without the boundary tax
Bind mounts on macOS are notoriously slow because every syscall crosses the hypervisor into APFS, where creating tens of thousands of tiny files incurs synchronous disk latency.

lighter approaches this differently:
- **Cached reads with change notifications:** The guest's page cache serves reads in memory without crossing the VM boundary. Host file changes invalidate guest cache entries through a notification channel. If macOS loses detailed events or the host queue overflows, a negotiated cache reset expires names, attributes and file pages. Delivery latency depends on host load; the [event-loss investigation](docs/filesystem-event-loss-2026-09-07.md) and benchmark samples record the observed behavior.
- **Asynchronous mutation lanes:** Creates, writes, and renames are promised to the guest immediately and flushed to APFS via dedicated asynchronous worker queues.
- **Identity-based inode tracking:** When descriptor limits are reached under massive trees (e.g. 100,000+ files in `node_modules`), inodes are parked and referenced through parent directory descriptors by identity, avoiding slow path walks and descriptor churn.

### 2. Fast container storage (`btrfs` with reflinks)
The container writable layer and named volumes live on an internal virtual disk (`~/.lighter/data.img`) formatted as `btrfs` with `nodatacow` and single metadata:
- **Instant clones:** File copies (`cp -a` or `yarn` cache links) use `copy_file_range` to reflink extents without copying physical bytes.
- **Inline completions:** A custom kernel patch allows checksum-free reads on `nodatacow` volumes to complete directly in the interrupt context rather than bouncing to worker threads.
- **Automatic reclamation:** Unused space is trimmed periodically and punched back out of the host image via `F_PUNCHHOLE`.

### 3. Cooperative memory management
A guest holding 8 GB of RAM after a heavy build starves the Mac.
- **A guest that is only as big as it needs to be:** The guest boots with a quarter of its configured memory and a virtio-mem range for the rest, plugged in 128 MiB blocks as the host offers them. A machine running no container shrinks toward its base, releasing page arrays with the removed blocks while retaining blocks that still hold unmovable kernel allocations; any container start makes the guest whole again before dockerd sees the request, so what runs inside sees the full `MemTotal` it always did.
- **Free page reporting:** `CONFIG_PAGE_REPORTING` volunteers unused guest pages. The host replaces their backing objects so surrendered pages return to macOS, and guest reuse is charged again. Live RAM stays nonvolatile; [accounting and physical-page experiments](docs/memory-accounting-2026-09-06.md) verify both paths.
- **Compressor-steered ballooning:** On memory-constrained hosts (like 8 GB M1s), macOS compresses memory before reporting pressure. lighter monitors the host compressor rate: if the Mac begins compressing heavily, the virtio-balloon inflates in aligned 16 KiB host-page compound blocks to safely release host physical memory, deflating once the compressor has quieted. Guest memory demand withdraws this compression-only target; actual macOS warning and critical pressure requests remain effective.

### 4. The network as streams, not packets
Every other runtime gives the VM a virtual network card and runs a TCP/IP stack on the Mac side to turn its packets back into connections. Each byte is then copied and checksummed twice, once by the guest kernel and once by that userspace stack, and every packet is a round trip across the hypervisor boundary.

lighter does not carry packets across the boundary at all:
- **One connection, one stream:** When a container opens a TCP connection, the guest kernel redirects it to lighter's agent, which opens a single vsock stream to the host for it. The host side opens an ordinary macOS socket to the destination and copies bytes between the two. The Mac's own kernel terminates the real connection, so VPNs, proxies and the Mac's routing all apply as they would to any Mac process, and there is no TCP/IP stack to maintain in lighter.
- **Joined in the guest kernel:** The container's socket and its vsock stream are joined by a BPF sockmap, so the data path inside the guest is a kernel-to-kernel copy with no process in the middle. Failed joins roll back before copying, or close the affected connections if forwarding has already started.
- **Published ports the same way:** A port a container publishes is bound on the Mac by lighter itself, and each accepted connection becomes a stream into the guest, where the kernel's own DNAT hands it to the container. Loopback-bound container publishes can also pass through Docker's guest proxy; the burst gate exercises both paths.
- **DNS answered on the Mac:** A container's lookups are resolved by the Mac's own resolver, so lookups follow the host's resolver configuration. The network tables measure the complete lookup path.
- **Low request latency:** After every event, the host thread that moves bytes keeps polling for a few tens of microseconds before it goes to sleep, so the reply that follows a request is picked up without waiting for the scheduler to wake it. The network tables report median and p99 HTTP latency on a kept-alive connection.

UDP takes the same stream, tagged per flow. What has no stream form, ARP, DHCP and ICMP, still reaches the virtual network card, and lighter answers those itself, in process: there is no network stack and no sidecar behind the card at all.

### 5. Starting up, and starting containers
Cold start includes allocating guest-memory metadata, starting Linux and waiting for Docker. Container-start timing measures a running VM; the tables report both separately.
- **A kernel that boots in fifty milliseconds:** Nothing is probed that a VM does not have, and the one library that benchmarked itself at boot (the raid6 code btrfs pulls in, 0.55 s of nine algorithms) is told which to use.
- **containerd first, in parallel:** The guest's init starts containerd the moment the data disk is mounted and points dockerd at it, instead of letting dockerd start its own and poll for it once a second. Everything waits in tens of milliseconds, not seconds: init on dockerd, the CLI on docker.
- **A flush is `fsync`:** A guest's disk flush becomes an `fsync` of the image, the data at the drive, which is what every Mac runtime gives a guest and takes tens of microseconds. Not the drive-cache commit Rust's standard library performs on macOS, which costs four milliseconds and which a container start would pay eighty times over.
- **Grace periods that do not wait for the clock:** Creating and tearing down a container's network waits on RCU grace periods, which end on the scheduler tick, and a container's life is a chain of them. The guest asks for the expedited kind where it can and tells the grace-period thread not to wait a jiffy before its first scan. The tick itself stays at 250 a second: a 1000 Hz kernel was measured beside it, and what it gave container starts it took from the share's installs, which are what most people do most of the time.
- **A stop that is a shutdown:** `lighter stop` asks the guest to stop the engine, sync and power off, in half a second, so nothing written in the last half minute is lost.

---

### 6. Linux LTS kernel strategy
lighter runs a custom Linux kernel tracking the official Longterm Support (LTS) tree (currently `6.18.49-lighter`). Rather than carrying a large out-of-tree fork, lighter maintains a minimal, audited patch set focused strictly on hypervisor integration and guest performance:
- **`virtio-mem` block page arrays and auto-movable onlining:** Each memory block carries its own page array (`0024`), and blocks online as movable only in proportion to kernel-usable RAM (`0026`), preventing slab exhaustion on small guests.
- **BPF sockmap backoff:** Prevents backlog worker spins when published sockets stall or linger with unread bytes (`0025`).
- **Apple Silicon TSO ordering:** Configures per-thread TSO memory ordering for high-performance Rosetta x86-64 execution without penalizing native ARM64 processes (`0023`).
- **`btrfs` inline completions:** Direct interrupt-context completion for checksum-free reads and writes on `nodatacow` volumes (`0009`).
- **Adaptive idle polling:** Bounded polling before WFI to eliminate cross-vCPU IPI latency during heavy multi-threaded builds (`0011`).

Kernel releases track upstream Linux LTS point updates, ensuring ongoing security patches, stability, and driver support without architectural churn.

---

## Benchmarks

Measured with the pinned 1,232-package fixture in `benchmarks/`. Timing rows are medians of three timed repetitions. The harness attempts an untimed installation for each package manager and an untimed npm install to materialize read and metadata inputs. Warm-up and per-repetition setup exit statuses were not retained; each valid timing case requires three successful measured repetitions. Those cases retain all three timings, including a potentially colder first read. Startup has an untimed round. Memory and power rows are single sampling windows.

Each host's Lighter 0.5.0 record is its first valid complete suite, selected before looking at results. Two further suites, five fresh same-build storage runs and an alternating comparison rebuilt from 0.4.1/0.5.0 source are retained in [the release measurements](benchmarks/RELEASE-0.5.0.md). Quiet checks precede each stage and competing-VM checks run throughout. Lighter uses eight guest CPUs on both hosts, with 4 GiB guest RAM on M1 and 16 GiB on M5. Competitor resource settings and actual guest topology are recorded alongside their measurements. Runtime and guest fingerprints, exact tool versions, image IDs and recording dates accompany the raw CSVs.

Native and container installations use the same pinned Node, npm, pnpm and Yarn versions. All container runtimes load identical benchmark images for each architecture. Native macOS and Linux utilities still differ, so the native ratios compare complete workloads rather than isolating filesystem overhead. Absolute times and percentages of native APFS are shown (higher percentages mean faster). The first storage table uses the runtime's own disk; the second uses a Mac directory shared into the container. Bold marks the lowest observed runtime median, without implying statistical significance. A dash means no completed measurement is available.

Colima on M1 repeatedly failed the host-share `pnpm install` case with `EMFILE` (too many open files). That cell is unavailable; its other complete cases come from the first share attempt, and its guest-disk cases were recorded separately. The release record retains the failed attempts and diagnostics. Docker Desktop is measured with Apple Virtualization.framework, VirtioFS and Rosetta; other Docker Desktop backends are outside this comparison.

The host-edit row measures polling visibility and a round trip; it is not an inotify or `fs.watch` event-delivery measurement.

Docker Desktop's host-share package cleanup failed on both hosts. The affected install timings and dependent storage/package-load memory results are excluded despite the original harness returning success; its guest-disk and independent cases remain. The release report retains the errors and explicit selection decisions.

### Apple M5 Pro (18 cores, 48 GB RAM)

| Workload (own disk) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 6.53 s | **4.61 s** (142%) | 6.83 s (96%) | 7.56 s (86%) | 7.96 s (82%) |
| `pnpm install` | 4.32 s | 1.22 s (355%) | 1.80 s (240%) | **1.06 s** (408%) | 2.75 s (157%) |
| `yarn install` | 5.89 s | **4.15 s** (142%) | 5.05 s (117%) | 6.00 s (98%) | 10.10 s (58%) |
| `ripgrep` (file read) | 927 ms | **79 ms** (1173%) | 118 ms (786%) | 115 ms (806%) | 126 ms (736%) |
| `find` (metadata walk) | 390 ms | **97 ms** (402%) | 129 ms (302%) | 185 ms (211%) | 124 ms (315%) |
| `cp -a node_modules` | 16.76 s | **937 ms** (1789%) | 1.05 s (1599%) | 1.35 s (1242%) | 2.49 s (673%) |
| `rm -rf node_modules` | 4.18 s | **387 ms** (1081%) | 473 ms (884%) | 488 ms (857%) | 397 ms (1053%) |

| Workload (host share) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 6.53 s | **6.30 s** (104%) | 8.53 s (77%) | 17.89 s (36%) | — |
| `pnpm install` | 4.32 s | **3.86 s** (112%) | 4.93 s (88%) | 25.95 s (17%) | — |
| `yarn install` | 5.89 s | **5.12 s** (115%) | 7.97 s (74%) | 22.81 s (26%) | — |
| `ripgrep` (file read) | 927 ms | **91 ms** (1019%) | 1.00 s (93%) | 3.02 s (31%) | — |
| `find` (metadata walk) | 390 ms | **95 ms** (411%) | 452 ms (86%) | 1.46 s (27%) | — |
| `cp -a node_modules` | 16.76 s | **3.59 s** (467%) | 9.58 s (175%) | 41.95 s (40%) | — |
| `rm -rf node_modules` | 4.18 s | **2.64 s** (158%) | 3.35 s (125%) | 8.38 s (50%) | — |
| Host file edit -> container | 1 ms | 6 ms | 11 ms | **2 ms** | 3 ms |

#### Memory footprint

The macOS physical-footprint charge for the runtime's own processes, corresponding to Activity Monitor's "Memory" column: idle a minute after a cold start, the peak during an `npm ci`, and 15 and 60 seconds after it ends. Lower is better. This includes compressed-memory charges and is not a count of distinct resident RAM. lighter 0.4.1 removes the duplicate charge when host and guest access the same backing pages, while preserving physical reclamation and charging reused pages again. This accounting correction does not imply an equivalent reduction in physical RAM. The idle and after rows include retained guest cache and host allocations. Configured RAM limits guest memory; host allocations add overhead. [Accounting and real-build experiments](docs/memory-accounting-2026-09-06.md) document the fix, compression and recovery at smaller configurations.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| Idle, a minute after start | **365 MiB** | 936 MiB | 1302 MiB | 3493 MiB |
| Peak through an npm install | **4386 MiB** | 5699 MiB | 10054 MiB | — |
| 15 s after it ends | **936 MiB** | 2776 MiB | 10145 MiB | — |
| 60 s after it ends | **936 MiB** | 1720 MiB | 10149 MiB | — |

#### The network

iperf3 between a container and the Mac in both directions, on the path a container sees (its egress to the Mac's LAN address) and on the path the Mac sees (a published port on localhost); then connection setup, request latency on a kept-alive connection, and DNS from inside a container. Connection rate counts client TCP handshakes; it is not completed HTTP requests per second. Throughput uses iperf’s received summary where available. Docker Desktop’s zero UDP receiver result was separately checked with raw JSON on this tested path. Bold marks the highest observed throughput or lowest latency, without a significance claim.

| Case | unit | native | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|---|
| TCP, container to the Mac | Gbit/s | 124.1 | 92.5 | **95.1** | 4.5 | 24.5 |
| TCP, the Mac to a container | Gbit/s | 131.2 | **84.9** | 51.5 | 4.0 | 14.7 |
| TCP into a published port | Gbit/s | — | **83.8** | 52.3 | 3.9 | 14.5 |
| TCP out of a published port | Gbit/s | — | **88.8** | 88.3 | 4.2 | 32.5 |
| UDP, container to the Mac | Gbit/s | 21.1 | **5.0** | 3.0 | 3.1 | 0.0 |
| connects to a published port | thousand per second | 26.6 | 16.8 | **21.2** | 16.1 | 15.7 |
| GET on a published port, median | µs | 40 | **60** | 74 | 229 | 125 |
| GET on a published port, p99 | µs | 68 | 154 | **132** | 366 | 232 |
| DNS lookup from a container, median | µs | 5611 | **37** | 262 | 481 | 513 |

#### Idle power

After a quiet minute, a minute of powermetrics samples over the runtime's processes: CPU as milliseconds of core per second, and wakeups per second. Lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| CPU, ms per second | 3 | **2** | 4 | 36 |
| Wakeups per second | 61 | 120 | **41** | 3873 |

#### Starting up

From a cold stop, the runtime asked to start the way a person would (`lighter start`, `orb start`, `colima start`, opening Docker Desktop): how long until `docker version` answers, and until the first container has run. Median of three; lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| Start until docker answers | 1.6 s | **1.1 s** | 8.8 s | 1.8 s |
| Start until the first container has run | 1.8 s | **1.4 s** | 9.0 s | 2.1 s |

#### x86-64 images

The same runtimes running `linux/amd64` images on their own disk: an install that mostly waits on the disk and the network, straight-line computation (a gigabyte through `sha256sum`), and a container's start, so the translator's price shows on each kind of work. lighter, OrbStack and Docker Desktop run these under Rosetta; Colima was started with `--vz-rosetta`. The first column is lighter's own arm64 number for the same case, for scale. Median of three; lower is better.

| Workload (x86-64 image, own disk) | lighter, arm64 | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 4.61 s | **9.24 s** | 13.14 s | 12.73 s | 14.28 s |
| `pnpm install` | 1.22 s | **2.73 s** | 3.58 s | 2.85 s | 3.85 s |
| `sha256sum` of 1 GiB | 3.01 s | **4.22 s** | 7.92 s | 4.26 s | 4.39 s |
| container start, `alpine true` | 152 ms | **155 ms** | 244 ms | 185 ms | 170 ms |

### Apple M1 (8 cores, 8 GB RAM)

| Workload (own disk) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 8.13 s | **7.58 s** (107%) | 9.15 s (89%) | 10.70 s (76%) | 11.91 s (68%) |
| `pnpm install` | 4.63 s | 1.76 s (263%) | 2.41 s (192%) | **1.64 s** (282%) | 2.18 s (213%) |
| `yarn install` | 9.66 s | 7.83 s (123%) | **7.77 s** (124%) | 10.88 s (89%) | 11.20 s (86%) |
| `ripgrep` (file read) | 1.23 s | **131 ms** (941%) | 155 ms (795%) | 206 ms (599%) | 209 ms (590%) |
| `find` (metadata walk) | 531 ms | **121 ms** (439%) | 130 ms (408%) | 219 ms (242%) | 149 ms (356%) |
| `cp -a node_modules` | 22.29 s | 4.05 s (550%) | 2.96 s (754%) | **2.21 s** (1007%) | 3.09 s (722%) |
| `rm -rf node_modules` | 5.48 s | **603 ms** (908%) | 649 ms (844%) | 787 ms (696%) | **603 ms** (908%) |

| Workload (host share) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 8.13 s | 11.23 s (72%) | **11.15 s** (73%) | 23.12 s (35%) | — |
| `pnpm install` | 4.63 s | 6.68 s (69%) | **6.19 s** (75%) | — | — |
| `yarn install` | 9.66 s | 10.93 s (88%) | **10.56 s** (91%) | 28.47 s (34%) | — |
| `ripgrep` (file read) | 1.23 s | **192 ms** (642%) | 1.11 s (111%) | 14.62 s (8%) | — |
| `find` (metadata walk) | 531 ms | **134 ms** (396%) | 526 ms (101%) | 3.84 s (14%) | — |
| `cp -a node_modules` | 22.29 s | **7.89 s** (283%) | 12.17 s (183%) | 60.42 s (37%) | — |
| `rm -rf node_modules` | 5.48 s | **2.67 s** (205%) | 4.08 s (134%) | 12.51 s (44%) | — |
| Host file edit -> container | 1 ms | 3 ms | 11 ms | **2 ms** | 3 ms |

#### Memory footprint

The macOS physical-footprint charge for the runtime's own processes, corresponding to Activity Monitor's "Memory" column: idle a minute after a cold start, the peak during an `npm ci`, and 15 and 60 seconds after it ends. Lower is better. This includes compressed-memory charges and is not a count of distinct resident RAM. lighter 0.4.1 removes the duplicate charge when host and guest access the same backing pages, while preserving physical reclamation and charging reused pages again. This accounting correction does not imply an equivalent reduction in physical RAM. The idle and after rows include retained guest cache and host allocations. Configured RAM limits guest memory; host allocations add overhead. [Accounting and real-build experiments](docs/memory-accounting-2026-09-06.md) document the fix, compression and recovery at smaller configurations.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| Idle, a minute after start | **248 MiB** | 578 MiB | 1135 MiB | 3200 MiB |
| Peak through an npm install | **4123 MiB** | 4373 MiB | 4340 MiB | — |
| 15 s after it ends | **813 MiB** | 2076 MiB | 4307 MiB | — |
| 60 s after it ends | **807 MiB** | 1309 MiB | 4315 MiB | — |

#### The network

iperf3 between a container and the Mac in both directions, on the path a container sees (its egress to the Mac's LAN address) and on the path the Mac sees (a published port on localhost); then connection setup, request latency on a kept-alive connection, and DNS from inside a container. Connection rate counts client TCP handshakes; it is not completed HTTP requests per second. Throughput uses iperf’s received summary where available. Docker Desktop’s zero UDP receiver result was separately checked with raw JSON on this tested path. Bold marks the highest observed throughput or lowest latency, without a significance claim.

| Case | unit | native | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|---|
| TCP, container to the Mac | Gbit/s | 112.7 | 55.8 | **64.8** | 4.3 | 13.9 |
| TCP, the Mac to a container | Gbit/s | 119.7 | **49.3** | 32.2 | 3.2 | 10.6 |
| TCP into a published port | Gbit/s | — | **48.2** | 32.4 | 3.1 | 10.3 |
| TCP out of a published port | Gbit/s | — | 54.0 | **67.1** | 3.7 | 22.6 |
| UDP, container to the Mac | Gbit/s | 22.7 | **4.8** | 3.3 | 2.5 | 0.0 |
| connects to a published port | thousand per second | 24.9 | 10.6 | **17.0** | 11.1 | 15.8 |
| GET on a published port, median | µs | 58 | 132 | **125** | 469 | 191 |
| GET on a published port, p99 | µs | 95 | 249 | **211** | 547 | 274 |
| DNS lookup from a container, median | µs | 3812 | **135** | 387 | 675 | 751 |

#### Idle power

After a quiet minute, a minute of powermetrics samples over the runtime's processes: CPU as milliseconds of core per second, and wakeups per second. Lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| CPU, ms per second | 6 | **4** | 11 | 46 |
| Wakeups per second | 51 | **14** | 52 | 2140 |

#### Starting up

From a cold stop, the runtime asked to start the way a person would (`lighter start`, `orb start`, `colima start`, opening Docker Desktop): how long until `docker version` answers, and until the first container has run. Median of three; lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| Start until docker answers | **0.8 s** | 1.2 s | 12.8 s | 3.1 s |
| Start until the first container has run | **1.0 s** | 1.5 s | 13.0 s | 3.6 s |

#### x86-64 images

The same runtimes running `linux/amd64` images on their own disk: an install that mostly waits on the disk and the network, straight-line computation (a gigabyte through `sha256sum`), and a container's start, so the translator's price shows on each kind of work. lighter, OrbStack and Docker Desktop run these under Rosetta; Colima was started with `--vz-rosetta`. The first column is lighter's own arm64 number for the same case, for scale. Median of three; lower is better.

| Workload (x86-64 image, own disk) | lighter, arm64 | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 7.58 s | **15.00 s** | 19.02 s | 19.35 s | 21.22 s |
| `pnpm install` | 1.76 s | 3.90 s | 4.47 s | **3.67 s** | 4.15 s |
| `sha256sum` of 1 GiB | 6.30 s | **7.18 s** | 12.92 s | 7.28 s | 7.31 s |
| container start, `alpine true` | 204 ms | **187 ms** | 257 ms | 201 ms | 219 ms |

[Release records](docs/records/0.5.0/benchmarks/) retain raw CSVs, case diagnostics, selection decisions and environment evidence. `benchmarks/RESULTS.md` contains individual repetition timings and methodology.
## What it does

- **Docker and Compose compatibility:** Full support via standard Docker CLI and Compose plugins.
- **Bidirectional port forwarding:** Published ports appear on the Mac the moment a container binds them, TCP and UDP, over IPv4 and IPv6, carried as streams rather than through a proxy. A publish binds where Docker's would: `-p 8080:80` on every interface, so another machine on your network can reach it; `-p 127.0.0.1:8080:80` on loopback only. `lighter config --publish localhost` keeps every publish on loopback on a Mac that should not offer its containers to the network it is on.
- **IPv6 in containers:** Every container has an IPv6 address and route, and reaches v6 destinations over TCP, UDP and ICMP exactly when your Mac can. On a network without IPv6, names resolve to IPv4 only, so nothing waits on an address that cannot be reached.
- **Native file sharing:** Mount any directory from your Mac with native ownership translation.
- **x86-64 containers under Rosetta:** `linux/amd64` images run under Apple's Rosetta, a one-time download (`lighter rosetta --install`). There is no emulator behind it; without Rosetta an amd64 container fails with that command in its output. [How, and what it costs](docs/x86-64.md).
- **Lean footprint:** Idles at roughly 0.2% CPU and hands memory back as soon as containers stop.

## Out of scope

- **GUI:** lighter runs headless in the background as a launchd service or terminal process.
- **Kubernetes:** Focused purely on fast Docker container workflows.
- **Intel Macs:** Built strictly for Apple Silicon (ARM64).
- **Windows or Linux hosts:** lighter is purpose-built for macOS.

---

## Architecture

The codebase is split into discrete crates, each responsible for one layer:

```text
lighter (CLI)  ──spawns──▶  lighter run
                                 │
                                 ├── lighter-hv       Safe Rust bindings to Hypervisor.framework
                                 ├── lighter-vmm      vCPUs, GICv3, device tree, memory layout, virtio
                                 ├── lighter-fs       virtio-fs host implementation, caching, FSEvents
                                 └── lighter-docker   Docker socket bridge and port forwarder
```

The guest environment consists of:
- A custom 6.18 longterm Linux kernel booting uncompressed directly from memory (no bootloader).
- Minimal Alpine-based root filesystem with `dockerd` and a lightweight Rust guest agent.

See [`docs/architecture.md`](docs/architecture.md) for detailed internals.

---

## Building from source

### Prerequisites

- Rust 1.85+
- Docker (only needed to build the guest kernel and rootfs images)
- macOS 15+ SDK with Hypervisor entitlement

### Build commands

```bash
# 1. Build guest kernel and rootfs
make guest

# 2. Build lighter CLI and VMM, ad-hoc signed with the hypervisor entitlement
make build

# 3. Run milestone verification gates
make gates
```

Milestone gates (`make gates`) boot real test VMs to verify end-to-end functionality (kernel boot, device negotiation, Docker engine readiness, network egress, shared filesystem coherency, and memory reclamation).

---

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option, as Rust projects conventionally are. Unless you say otherwise, a contribution you submit for inclusion is licensed the same way, without further terms.
