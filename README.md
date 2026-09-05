# lighter

The fastest Docker runtime for macOS, open-source.

lighter is a virtual machine monitor built directly on `Hypervisor.framework` in Rust, implementing its own vCPU loop, GICv3 interrupt controller, virtio device models, and guest Linux kernel. Built from scratch to be the fastest way to run containers on a Mac, lighter is an open-source, MIT-licensed competitor to OrbStack and Docker Desktop.

**MIT licensed. No commercial subscriptions, no paid tiers, no "free during beta", and no telemetry.**

Apple Silicon, macOS 15 (Sequoia) or later.

---

## Install

### The one-line installer

```bash
curl -fsSL https://raw.githubusercontent.com/fieldwork-ai/lighter/main/scripts/install.sh | sh
```

### Or via Homebrew

```bash
brew tap fieldwork-ai/tap
brew install lighter
```

Then start the daemon:

```bash
lighter start
```

`lighter start` boots the VM in under half a second and registers a Docker CLI context. Your existing `docker` and `docker compose` commands point at it immediately, with nothing to export and no manual socket flags.

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

## Benchmarks

Measured on clean machines against a 1,232-package `package.json` fixture (`benchmarks/`). Each figure is the median of three timed repetitions, following an untimed warm-up run. Numbers are reported as absolute time and as a percentage of native APFS on the same machine (higher means faster). The first table is the runtime's own disk, where a container's writable layer and its volumes live; the second is a host share, the Mac's directory bind-mounted into the container. Bold marks the fastest runtime in each row; a dash is a case the runtime could not complete.

OrbStack, Colima and Docker Desktop were measured on the same machines in the same sessions.

### Apple M5 Pro (18 cores, 48 GB RAM)

| Workload (own disk) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 6.16 s | **4.74 s** (130%) | 7.01 s (88%) | 8.59 s (72%) | 8.61 s (72%) |
| `pnpm install` | 3.77 s | 1.32 s (286%) | 2.03 s (185%) | **1.14 s** (332%) | 2.87 s (131%) |
| `yarn install` | 5.75 s | **4.07 s** (141%) | 5.08 s (113%) | 6.58 s (87%) | 11.14 s (52%) |
| `ripgrep` (file read) | 927 ms | **79 ms** (1173%) | 102 ms (909%) | 121 ms (766%) | 124 ms (748%) |
| `find` (metadata walk) | 357 ms | **97 ms** (368%) | 127 ms (281%) | 176 ms (203%) | 131 ms (273%) |
| `cp -a node_modules` | 13.55 s | **938 ms** (1445%) | 1.11 s (1216%) | 1.88 s (722%) | 2.58 s (526%) |
| `rm -rf node_modules` | 3.65 s | **378 ms** (967%) | 496 ms (737%) | 551 ms (663%) | 428 ms (854%) |

| Workload (host share) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 6.16 s | **6.29 s** (98%) | 8.49 s (73%) | 17.79 s (35%) | 17.91 s (34%) |
| `pnpm install` | 3.77 s | **3.69 s** (102%) | 4.72 s (80%) | 25.43 s (15%) | 28.34 s (13%) |
| `yarn install` | 5.75 s | **5.20 s** (111%) | 7.79 s (74%) | 22.16 s (26%) | 22.58 s (25%) |
| `ripgrep` (file read) | 927 ms | **79 ms** (1173%) | 1.02 s (91%) | 6.86 s (14%) | 9.84 s (9%) |
| `find` (metadata walk) | 357 ms | **87 ms** (410%) | 595 ms (60%) | 1.43 s (25%) | 1.88 s (19%) |
| `cp -a node_modules` | 13.55 s | **3.29 s** (412%) | 8.71 s (156%) | 44.30 s (31%) | 33.55 s (40%) |
| `rm -rf node_modules` | 3.65 s | **2.39 s** (153%) | 2.97 s (123%) | 8.05 s (45%) | 6.56 s (56%) |
| Host file edit -> container | 2 ms | **2 ms** | — | 1.00 s | 1.00 s |

#### Memory footprint

The physical footprint of the runtime's own processes, which is the "Memory" column in Activity Monitor: settled before an `npm ci`, at its peak during one, and 15 and 60 seconds after it ends with nothing running. Lower is better throughout.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| Settled, before an install | 1158 MiB | **1028 MiB** | 8208 MiB | 9179 MiB |
| Peak through an npm install | **1158 MiB** | 5498 MiB | 8700 MiB | 9182 MiB |
| 15 s after it ends | **872 MiB** | 2850 MiB | 8735 MiB | 9187 MiB |
| 60 s after it ends | **862 MiB** | 2114 MiB | 8735 MiB | 9187 MiB |

#### The network

iperf3 between a container and the Mac in both directions, on the path a container sees (its egress to the Mac's LAN address) and on the path the Mac sees (a published port on localhost); then connection setup, request latency on a kept-alive connection, and DNS from inside a container. Bold marks the best runtime in each row.

| Case | unit | native | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|---|
| TCP, container to the Mac | Gbit/s | 123.5 | **98.5** | 97.2 | 4.5 | 23.2 |
| TCP, the Mac to a container | Gbit/s | 129.2 | **90.1** | 52.9 | 3.9 | 14.3 |
| TCP into a published port | Gbit/s | — | **97.7** | 54.2 | 3.8 | 14.3 |
| TCP out of a published port | Gbit/s | — | 89.9 | **93.1** | 4.4 | 33.4 |
| UDP, container to the Mac | Gbit/s | 21.8 | **5.0** | 3.1 | 3.3 | 0.0 |
| connects to a published port | thousand per second | 26.0 | **17.6** | 16.2 | 15.8 | 17.0 |
| GET on a published port, median | µs | 40 | **68** | 73 | 224 | 119 |
| GET on a published port, p99 | µs | 70 | 220 | **119** | 361 | 245 |
| DNS lookup from a container, median | µs | 2850 | **55** | 251 | 483 | 474 |

#### Idle power

After a quiet minute, a minute of powermetrics samples over the runtime's processes: CPU as milliseconds of core per second, and wakeups per second. Lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| CPU, ms per second | 5 | **2** | 5 | 25 |
| Wakeups per second | 98 | 99 | **50** | 3748 |

### Apple M1 (8 cores, 8 GB RAM)

| Workload (own disk) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 7.68 s | **7.96 s** (96%) | 10.39 s (74%) | 11.30 s (68%) | 12.60 s (61%) |
| `pnpm install` | 4.47 s | 2.04 s (219%) | 2.44 s (183%) | **1.56 s** (286%) | 2.23 s (201%) |
| `yarn install` | 9.62 s | **7.84 s** (123%) | 7.88 s (122%) | 11.55 s (83%) | 11.74 s (82%) |
| `ripgrep` (file read) | 1.21 s | **132 ms** (917%) | 160 ms (756%) | 183 ms (661%) | 260 ms (465%) |
| `find` (metadata walk) | 512 ms | **128 ms** (400%) | 132 ms (388%) | 215 ms (238%) | 152 ms (337%) |
| `cp -a node_modules` | 21.50 s | **3.12 s** (689%) | 3.21 s (669%) | 3.26 s (660%) | 6.20 s (347%) |
| `rm -rf node_modules` | 5.37 s | 602 ms (893%) | 736 ms (730%) | 729 ms (737%) | **592 ms** (908%) |

| Workload (host share) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 7.68 s | **11.19 s** (69%) | 11.54 s (67%) | 23.58 s (33%) | 25.85 s (30%) |
| `pnpm install` | 4.47 s | **5.53 s** (81%) | 5.89 s (76%) | — | 46.36 s (10%) |
| `yarn install` | 9.62 s | 10.54 s (91%) | **10.36 s** (93%) | 28.10 s (34%) | 35.38 s (27%) |
| `ripgrep` (file read) | 1.21 s | **193 ms** (627%) | 1.11 s (109%) | 17.04 s (7%) | 13.24 s (9%) |
| `find` (metadata walk) | 512 ms | **121 ms** (423%) | 552 ms (93%) | 3.81 s (13%) | 4.10 s (12%) |
| `cp -a node_modules` | 21.50 s | **5.32 s** (404%) | 15.19 s (142%) | 62.92 s (34%) | 44.64 s (48%) |
| `rm -rf node_modules` | 5.37 s | **3.46 s** (155%) | 3.97 s (135%) | 12.38 s (43%) | 12.78 s (42%) |
| Host file edit -> container | 2 ms | **2 ms** | **2 ms** | 6 ms | 10 ms |

#### Memory footprint

The physical footprint of the runtime's own processes, which is the "Memory" column in Activity Monitor: settled before an `npm ci`, at its peak during one, and 15 and 60 seconds after it ends with nothing running. Lower is better throughout.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| Settled, before an install | 2353 MiB | **1097 MiB** | 4343 MiB | 4503 MiB |
| Peak through an npm install | **3863 MiB** | 4112 MiB | 4344 MiB | 4503 MiB |
| 15 s after it ends | **1645 MiB** | 2144 MiB | 4316 MiB | 4472 MiB |
| 60 s after it ends | 1566 MiB | **1027 MiB** | 4315 MiB | 4472 MiB |

#### The network

iperf3 between a container and the Mac in both directions, on the path a container sees (its egress to the Mac's LAN address) and on the path the Mac sees (a published port on localhost); then connection setup, request latency on a kept-alive connection, and DNS from inside a container. Bold marks the best runtime in each row.

| Case | unit | native | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|---|
| TCP, container to the Mac | Gbit/s | 119.0 | 52.7 | **63.6** | 4.3 | 13.6 |
| TCP, the Mac to a container | Gbit/s | 118.0 | **32.9** | 30.3 | 3.2 | 10.4 |
| TCP into a published port | Gbit/s | — | **43.5** | 29.9 | 3.0 | 10.3 |
| TCP out of a published port | Gbit/s | — | 54.5 | **67.5** | 3.8 | 22.4 |
| UDP, container to the Mac | Gbit/s | 24.5 | **4.9** | 3.1 | 2.6 | 0.0 |
| connects to a published port | thousand per second | 25.5 | 14.9 | **16.5** | 9.1 | 16.2 |
| GET on a published port, median | µs | 54 | 133 | **128** | 464 | 191 |
| GET on a published port, p99 | µs | 97 | 242 | **231** | 539 | 497 |
| DNS lookup from a container, median | µs | 3851 | **141** | 422 | 714 | 765 |

#### Idle power

After a quiet minute, a minute of powermetrics samples over the runtime's processes: CPU as milliseconds of core per second, and wakeups per second. Lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| CPU, ms per second | 24 | 21 | **12** | 42 |
| Wakeups per second | 329 | 91 | **59** | 2011 |

`benchmarks/RESULTS.md` contains the full logs, individual repetition timings, and methodology.
## Why it is fast

The performance of containers on macOS comes down to five bottlenecks: the shared filesystem, virtual disk I/O, memory management, the network, and the time between asking for a container and having one.

### 1. Shared filesystems without the boundary tax
Bind mounts on macOS are notoriously slow because every syscall crosses the hypervisor into APFS, where creating tens of thousands of tiny files incurs synchronous disk latency.

lighter approaches this differently:
- **Cached reads with real-time invalidation:** The guest's page cache serves reads in memory without crossing the VM boundary. To keep it coherent with macOS edits, lighter's custom virtio-fs driver incorporates a notification channel: when macOS `FSEvents` detects a host file change, it invalidates the exact guest dentry within milliseconds.
- **Asynchronous mutation lanes:** Creates, writes, and renames are promised to the guest immediately and flushed to APFS via dedicated asynchronous worker queues.
- **Identity-based inode tracking:** When descriptor limits are reached under massive trees (e.g. 100,000+ files in `node_modules`), inodes are parked and referenced through parent directory descriptors by identity, avoiding slow path walks and descriptor churn.

### 2. Fast container storage (`btrfs` with reflinks)
The container writable layer and named volumes live on an internal virtual disk (`~/.lighter/data.img`) formatted as `btrfs` with `nodatacow` and single metadata:
- **Instant clones:** File copies (`cp -a` or `yarn` cache links) use `copy_file_range` to reflink extents without copying physical bytes.
- **Inline completions:** A custom kernel patch allows checksum-free reads on `nodatacow` volumes to complete directly in the interrupt context rather than bouncing to worker threads.
- **Automatic reclamation:** Unused space is trimmed periodically and punched back out of the host image via `F_PUNCHHOLE`.

### 3. Cooperative memory management
A guest holding 8 GB of RAM after a heavy build starves the Mac.
- **Free page reporting:** When memory is freed inside the guest, `CONFIG_PAGE_REPORTING` volunteers those physical pages back to macOS immediately without hypervisor intervention.
- **Compressor-steered ballooning:** On memory-constrained hosts (like 8 GB M1s), macOS compresses memory before reporting pressure. lighter monitors the host compressor rate: if the Mac begins compressing heavily, the virtio-balloon inflates in aligned 16 KiB host-page compound blocks to safely release host physical memory, deflating once the compressor has quieted.

### 4. The network as streams, not packets
Every other runtime gives the VM a virtual network card and runs a TCP/IP stack on the Mac side to turn its packets back into connections. Each byte is then copied and checksummed twice, once by the guest kernel and once by that userspace stack, and every packet is a round trip across the hypervisor boundary.

lighter does not carry packets across the boundary at all:
- **One connection, one stream:** When a container opens a TCP connection, the guest kernel redirects it to lighter's agent, which opens a single vsock stream to the host for it. The host side opens an ordinary macOS socket to the destination and copies bytes between the two. The Mac's own kernel terminates the real connection, so VPNs, proxies and the Mac's routing all apply as they would to any Mac process, and there is no TCP/IP stack to maintain in lighter.
- **Joined in the guest kernel:** The container's socket and its vsock stream are joined by a BPF sockmap, so the data path inside the guest is a kernel-to-kernel copy with no process in the middle. This is where the throughput comes from: 98 Gbit/s out of a container on an M5 Pro, and 90 into one, against 97 and 53 for OrbStack.
- **Published ports the same way:** A port a container publishes is bound on the Mac by lighter itself, and each accepted connection becomes a stream into the guest. There is no userland proxy inside the VM to double-copy every byte.
- **DNS answered on the Mac:** A container's lookups are resolved by the Mac's own resolver, so split DNS from a VPN works and a lookup costs 55 µs instead of a trip through a virtual network.
- **Low request latency:** After every event, the host thread that moves bytes keeps polling for a few tens of microseconds before it goes to sleep, so the reply that follows a request is picked up without waiting for the scheduler to wake it. A GET on a published port costs 68 µs on the M5 and 133 µs on an M1, against 73 and 128 for OrbStack.

UDP takes the same stream, tagged per flow. What has no stream form, ARP, DHCP and ICMP, still reaches the virtual network card, and lighter answers those itself, in process: there is no network stack and no sidecar behind the card at all.

### 5. Starting up, and starting containers
`lighter start` answers `docker version` in under half a second, and a container runs in about a tenth of one.
- **A kernel that boots in fifty milliseconds:** Nothing is probed that a VM does not have, and the one library that benchmarked itself at boot (the raid6 code btrfs pulls in, 0.55 s of nine algorithms) is told which to use.
- **containerd first, in parallel:** The guest's init starts containerd the moment the data disk is mounted and points dockerd at it, instead of letting dockerd start its own and poll for it once a second. Everything waits in tens of milliseconds, not seconds: init on dockerd, the CLI on docker.
- **A flush is `fsync`:** A guest's disk flush becomes an `fsync` of the image, the data at the drive, which is what every Mac runtime gives a guest and takes tens of microseconds. Not the drive-cache commit Rust's standard library performs on macOS, which costs four milliseconds and which a container start would pay eighty times over.
- **A thousand ticks a second:** The waits inside the block layer, the scheduler and the network stack are measured in jiffies, and a container start is a chain of them; idle cores stop the tick, so nothing is paid for it at rest.
- **A stop that is a shutdown:** `lighter stop` asks the guest to stop the engine, sync and power off, in half a second, so nothing written in the last half minute is lost.

---

## What it does

- **Docker and Compose compatibility:** Full support via standard Docker CLI and Compose plugins.
- **Bidirectional port forwarding:** Published ports appear on `localhost` the moment a container binds them, carried as streams rather than through a proxy.
- **Native file sharing:** Mount any directory from your Mac with native ownership translation.
- **x86-64 containers under Rosetta:** `linux/amd64` images run under Apple's Rosetta when the Mac has it (`lighter rosetta --install`), and under `qemu-user` otherwise. [How, and what it costs](docs/x86-64.md).
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

MIT. See [LICENSE](LICENSE).
