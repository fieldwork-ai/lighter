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
| `npm ci` | 6.16 s | **4.46 s** (138%) | 7.01 s (88%) | 8.59 s (72%) | 8.61 s (72%) |
| `pnpm install` | 3.77 s | **1.12 s** (337%) | 2.03 s (185%) | 1.14 s (332%) | 2.87 s (131%) |
| `yarn install` | 5.75 s | **4.02 s** (143%) | 5.08 s (113%) | 6.58 s (87%) | 11.14 s (52%) |
| `ripgrep` (file read) | 927 ms | **78 ms** (1188%) | 102 ms (909%) | 121 ms (766%) | 124 ms (748%) |
| `find` (metadata walk) | 357 ms | **98 ms** (364%) | 127 ms (281%) | 176 ms (203%) | 131 ms (273%) |
| `cp -a node_modules` | 13.55 s | **902 ms** (1503%) | 1.11 s (1216%) | 1.88 s (722%) | 2.58 s (526%) |
| `rm -rf node_modules` | 3.65 s | **383 ms** (954%) | 496 ms (737%) | 551 ms (663%) | 428 ms (854%) |

| Workload (host share) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 6.16 s | **6.41 s** (96%) | 8.49 s (73%) | 17.79 s (35%) | 17.91 s (34%) |
| `pnpm install` | 3.77 s | **4.06 s** (93%) | 4.72 s (80%) | 25.43 s (15%) | 28.34 s (13%) |
| `yarn install` | 5.75 s | **5.25 s** (110%) | 7.79 s (74%) | 22.16 s (26%) | 22.58 s (25%) |
| `ripgrep` (file read) | 927 ms | **89 ms** (1042%) | 1.02 s (91%) | 6.86 s (14%) | 9.84 s (9%) |
| `find` (metadata walk) | 357 ms | **94 ms** (380%) | 595 ms (60%) | 1.43 s (25%) | 1.88 s (19%) |
| `cp -a node_modules` | 13.55 s | **3.58 s** (378%) | 8.71 s (156%) | 44.30 s (31%) | 33.55 s (40%) |
| `rm -rf node_modules` | 3.65 s | **2.71 s** (135%) | 2.97 s (123%) | 8.05 s (45%) | 6.56 s (56%) |
| Host file edit -> container | 2 ms | **2 ms** | — | 1.00 s | 1.00 s |

#### Memory footprint

The physical footprint of the runtime's own processes, which is the "Memory" column in Activity Monitor: idle a minute after a cold start with nothing run on it, at its peak during an `npm ci`, and 15 and 60 seconds after that ends with nothing running. Lower is better throughout.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| Idle, a minute after start | **635 MiB** | 1197 MiB | 11361 MiB | 2111 MiB |
| Peak through an npm install | **4168 MiB** | 5498 MiB | 8700 MiB | 9182 MiB |
| 15 s after it ends | **874 MiB** | 2850 MiB | 8735 MiB | 9187 MiB |
| 60 s after it ends | **850 MiB** | 2114 MiB | 8735 MiB | 9187 MiB |

#### The network

iperf3 between a container and the Mac in both directions, on the path a container sees (its egress to the Mac's LAN address) and on the path the Mac sees (a published port on localhost); then connection setup, request latency on a kept-alive connection, and DNS from inside a container. Bold marks the best runtime in each row.

| Case | unit | native | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|---|
| TCP, container to the Mac | Gbit/s | 123.5 | 93.3 | **97.2** | 4.5 | 23.2 |
| TCP, the Mac to a container | Gbit/s | 129.2 | **82.1** | 52.9 | 3.9 | 14.3 |
| TCP into a published port | Gbit/s | — | **91.1** | 54.2 | 3.8 | 14.3 |
| TCP out of a published port | Gbit/s | — | 87.3 | **93.1** | 4.4 | 33.4 |
| UDP, container to the Mac | Gbit/s | 21.8 | **5.0** | 3.1 | 3.3 | 0.0 |
| connects to a published port | thousand per second | 26.0 | **17.3** | 16.2 | 15.8 | 17.0 |
| GET on a published port, median | µs | 40 | **63** | 73 | 224 | 119 |
| GET on a published port, p99 | µs | 70 | 144 | **119** | 361 | 245 |
| DNS lookup from a container, median | µs | 2850 | **63** | 251 | 483 | 474 |

#### Idle power

After a quiet minute, a minute of powermetrics samples over the runtime's processes: CPU as milliseconds of core per second, and wakeups per second. Lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| CPU, ms per second | 6 | **2** | 5 | 25 |
| Wakeups per second | 126 | 99 | **50** | 3748 |

#### Starting up

From a cold stop, the runtime asked to start the way a person would (`lighter start`, `orb start`, `colima start`, opening Docker Desktop): how long until `docker version` answers, and until the first container has run. Median of three; lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| Start until docker answers | **0.4 s** | 1.5 s | 11.8 s | 2.1 s |
| Start until the first container has run | **0.6 s** | 1.9 s | 12.0 s | 2.4 s |

#### x86-64 images

The same runtimes running `linux/amd64` images on their own disk: an install that mostly waits on the disk and the network, straight-line computation (a gigabyte through `sha256sum`), and a container's start, so the translator's price shows on each kind of work. lighter, OrbStack and Docker Desktop run these under Rosetta; Colima was started with `--vz-rosetta`. The first column is lighter's own arm64 number for the same case, for scale. Median of three; lower is better.

| Workload (x86-64 image, own disk) | lighter, arm64 | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 4.46 s | **9.28 s** | 13.16 s | 12.71 s | 14.45 s |
| `pnpm install` | 1.12 s | 2.98 s | 4.44 s | **2.70 s** | 3.89 s |
| `sha256sum` of 1 GiB | 2.99 s | **4.17 s** | 8.01 s | 4.30 s | 4.44 s |
| container start, `alpine true` | 181 ms | **155 ms** | 280 ms | 180 ms | 165 ms |

### Apple M1 (8 cores, 8 GB RAM)

| Workload (own disk) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 7.81 s | **8.71 s** (90%) | 9.67 s (81%) | 11.44 s (68%) | 12.60 s (62%) |
| `pnpm install` | 4.38 s | 1.78 s (246%) | 2.40 s (182%) | **1.57 s** (278%) | 2.23 s (197%) |
| `yarn install` | 10.44 s | 7.92 s (132%) | **7.87 s** (133%) | 10.80 s (97%) | 11.74 s (89%) |
| `ripgrep` (file read) | 1.21 s | **137 ms** (882%) | 143 ms (845%) | 171 ms (707%) | 260 ms (465%) |
| `find` (metadata walk) | 510 ms | **125 ms** (408%) | 138 ms (370%) | 214 ms (238%) | 152 ms (336%) |
| `cp -a node_modules` | 24.53 s | 3.08 s (795%) | **2.49 s** (986%) | 2.71 s (905%) | 6.20 s (396%) |
| `rm -rf node_modules` | 5.38 s | 606 ms (887%) | 667 ms (806%) | 829 ms (649%) | **592 ms** (908%) |

| Workload (host share) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 7.81 s | 11.62 s (67%) | **11.25 s** (69%) | 23.00 s (34%) | 25.46 s (31%) |
| `pnpm install` | 4.38 s | 6.01 s (73%) | **5.91 s** (74%) | — | 45.94 s (10%) |
| `yarn install` | 10.44 s | 10.63 s (98%) | **10.43 s** (100%) | 28.11 s (37%) | 35.44 s (29%) |
| `ripgrep` (file read) | 1.21 s | **182 ms** (664%) | 1.09 s (110%) | 15.36 s (8%) | 13.15 s (9%) |
| `find` (metadata walk) | 510 ms | **123 ms** (415%) | 525 ms (97%) | 3.81 s (13%) | 4.10 s (12%) |
| `cp -a node_modules` | 24.53 s | **5.28 s** (464%) | 16.13 s (152%) | 59.57 s (41%) | 45.90 s (53%) |
| `rm -rf node_modules` | 5.38 s | **2.54 s** (211%) | 3.92 s (137%) | 12.45 s (43%) | 12.82 s (42%) |
| Host file edit -> container | 2 ms | 3 ms | 3 ms | **2 ms** | 12 ms |

#### Memory footprint

The physical footprint of the runtime's own processes, which is the "Memory" column in Activity Monitor: idle a minute after a cold start with nothing run on it, at its peak during an `npm ci`, and 15 and 60 seconds after that ends with nothing running. Lower is better throughout.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| Idle, a minute after start | **362 MiB** | 733 MiB | 1145 MiB | 1753 MiB |
| Peak through an npm install | **3426 MiB** | 4302 MiB | 4364 MiB | 4505 MiB |
| 15 s after it ends | **624 MiB** | 1876 MiB | 4337 MiB | 4473 MiB |
| 60 s after it ends | **616 MiB** | 1480 MiB | 4337 MiB | 4472 MiB |

#### The network

iperf3 between a container and the Mac in both directions, on the path a container sees (its egress to the Mac's LAN address) and on the path the Mac sees (a published port on localhost); then connection setup, request latency on a kept-alive connection, and DNS from inside a container. Bold marks the best runtime in each row.

| Case | unit | native | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|---|
| TCP, container to the Mac | Gbit/s | 117.7 | 53.8 | **64.9** | 4.3 | 13.6 |
| TCP, the Mac to a container | Gbit/s | 117.0 | **34.7** | 29.2 | 3.2 | 10.1 |
| TCP into a published port | Gbit/s | — | **43.1** | 29.9 | 3.1 | 10.1 |
| TCP out of a published port | Gbit/s | — | 54.6 | **67.6** | 3.8 | 22.2 |
| UDP, container to the Mac | Gbit/s | 24.4 | **5.0** | 3.1 | 2.6 | 0.0 |
| connects to a published port | thousand per second | 24.9 | **19.1** | 16.4 | 7.9 | 17.7 |
| GET on a published port, median | µs | 54 | 133 | **127** | 453 | 153 |
| GET on a published port, p99 | µs | 106 | 230 | **221** | 574 | 372 |
| DNS lookup from a container, median | µs | 3876 | **153** | 425 | 686 | 758 |

#### Idle power

After a quiet minute, a minute of powermetrics samples over the runtime's processes: CPU as milliseconds of core per second, and wakeups per second. Lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| CPU, ms per second | **10** | 20 | 11 | 44 |
| Wakeups per second | 103 | 84 | **55** | 1998 |

#### Starting up

From a cold stop, the runtime asked to start the way a person would (`lighter start`, `orb start`, `colima start`, opening Docker Desktop): how long until `docker version` answers, and until the first container has run. Median of three; lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| Start until docker answers | **0.5 s** | 1.2 s | 9.8 s | 2.7 s |
| Start until the first container has run | **0.7 s** | 1.5 s | 10.0 s | 3.2 s |

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
- **Joined in the guest kernel:** The container's socket and its vsock stream are joined by a BPF sockmap, so the data path inside the guest is a kernel-to-kernel copy with no process in the middle. This is where the throughput comes from: 93 Gbit/s out of a container on an M5 Pro, and 82 into one, against 97 and 53 for OrbStack.
- **Published ports the same way:** A port a container publishes is bound on the Mac by lighter itself, and each accepted connection becomes a stream into the guest, where the kernel's own DNAT hands it to the container. No proxy process inside the VM copies the bytes.
- **DNS answered on the Mac:** A container's lookups are resolved by the Mac's own resolver, so split DNS from a VPN works and a lookup costs about 60 µs instead of a trip through a virtual network.
- **Low request latency:** After every event, the host thread that moves bytes keeps polling for a few tens of microseconds before it goes to sleep, so the reply that follows a request is picked up without waiting for the scheduler to wake it. A GET on a published port costs 63 µs on the M5 and 133 µs on an M1, against 73 and 127 for OrbStack.

UDP takes the same stream, tagged per flow. What has no stream form, ARP, DHCP and ICMP, still reaches the virtual network card, and lighter answers those itself, in process: there is no network stack and no sidecar behind the card at all.

### 5. Starting up, and starting containers
`lighter start` answers `docker version` in under half a second, and a container runs in about a tenth of one.
- **A kernel that boots in fifty milliseconds:** Nothing is probed that a VM does not have, and the one library that benchmarked itself at boot (the raid6 code btrfs pulls in, 0.55 s of nine algorithms) is told which to use.
- **containerd first, in parallel:** The guest's init starts containerd the moment the data disk is mounted and points dockerd at it, instead of letting dockerd start its own and poll for it once a second. Everything waits in tens of milliseconds, not seconds: init on dockerd, the CLI on docker.
- **A flush is `fsync`:** A guest's disk flush becomes an `fsync` of the image, the data at the drive, which is what every Mac runtime gives a guest and takes tens of microseconds. Not the drive-cache commit Rust's standard library performs on macOS, which costs four milliseconds and which a container start would pay eighty times over.
- **Grace periods that do not wait for the clock:** Creating and tearing down a container's network waits on RCU grace periods, which are counted in ticks, and a container start is a chain of them. The guest asks for the expedited kind, which complete in microseconds, and keeps the tick itself at 250 a second: every tick is an exit from the VM, and on an M1 whose vCPUs fill its cores a thousand of them cost more than they saved.
- **A stop that is a shutdown:** `lighter stop` asks the guest to stop the engine, sync and power off, in half a second, so nothing written in the last half minute is lost.

---

## What it does

- **Docker and Compose compatibility:** Full support via standard Docker CLI and Compose plugins.
- **Bidirectional port forwarding:** Published ports appear on `localhost` the moment a container binds them, carried as streams rather than through a proxy.
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

MIT. See [LICENSE](LICENSE).
