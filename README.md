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

`lighter start` boots the VM in under two seconds and registers a Docker CLI context. Your existing `docker` and `docker compose` commands point at it immediately, with nothing to export and no manual socket flags.

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

Measured on clean machines against a 1,232-package `package.json` fixture (`benchmarks/`). Each figure is the median of three timed repetitions, following an untimed warm-up run. Numbers are reported as absolute time and as a percentage of native APFS on the same machine (higher means faster). Bold marks the fastest runtime in each row; a dash is a case the runtime could not complete.

OrbStack, Colima and Docker Desktop were measured on the same machines in the same sessions, not quoted from marketing materials.

### Apple M5 Pro (18 cores, 48 GB RAM)

| Workload | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 6.16 s | **6.29 s** (98%) | 8.49 s (73%) | 17.79 s (35%) | 17.91 s (34%) |
| `pnpm install` | 3.77 s | **3.69 s** (102%) | 4.72 s (80%) | 25.43 s (15%) | 28.34 s (13%) |
| `yarn install` | 5.75 s | **5.20 s** (111%) | 7.79 s (74%) | 22.16 s (26%) | 22.58 s (25%) |
| `ripgrep` (file read) | 927 ms | **79 ms** (1173%) | 1.02 s (91%) | 6.86 s (14%) | 9.84 s (9%) |
| `find` (metadata walk) | 357 ms | **87 ms** (410%) | 595 ms (60%) | 1.43 s (25%) | 1.88 s (19%) |
| `cp -a node_modules` | 13.55 s | **3.29 s** (412%) | 8.71 s (156%) | 44.30 s (31%) | 33.55 s (40%) |
| `rm -rf node_modules` | 3.65 s | **2.39 s** (153%) | 2.97 s (123%) | 8.05 s (45%) | 6.56 s (56%) |
| Host file edit -> container | 2 ms | **2 ms** | — | 1.00 s | 1.00 s |

| Workload (own disk) | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| `npm ci` | **4.74 s** | 7.01 s | 8.59 s | 8.61 s |
| `pnpm install` | 1.32 s | 2.03 s | **1.14 s** | 2.87 s |
| `yarn install` | **4.07 s** | 5.08 s | 6.58 s | 11.14 s |
| `ripgrep` (file read) | **79 ms** | 102 ms | 121 ms | 124 ms |
| `find` (metadata walk) | **97 ms** | 127 ms | 176 ms | 131 ms |
| `cp -a node_modules` | **938 ms** | 1.11 s | 1.88 s | 2.58 s |
| `rm -rf node_modules` | **378 ms** | 496 ms | 551 ms | 428 ms |

#### What the runtime costs the Mac

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
| TCP, container to the Mac | Mbit/s | 123472 | 95564 | **97161** | 4479 | 23245 |
| TCP, the Mac to a container | Mbit/s | 129208 | **85270** | 52870 | 3927 | 14257 |
| TCP into a published port | Mbit/s | — | **87786** | 54227 | 3838 | 14314 |
| TCP out of a published port | Mbit/s | — | 84353 | **93100** | 4357 | 33392 |
| UDP, container to the Mac | Mbit/s | 21838 | **4983** | 3123 | 3283 | 0 |
| connects to a published port | per second | 25963 | **17656** | 16155 | 15819 | 16994 |
| GET on a published port, median | µs | 40 | **58** | 73 | 224 | 119 |
| GET on a published port, p99 | µs | 70 | 153 | **119** | 361 | 245 |
| DNS lookup from a container, median | µs | 2850 | **54** | 251 | 483 | 474 |

#### What an idle runtime costs

After a quiet minute, a minute of powermetrics samples over the runtime's processes: CPU as milliseconds of core per second, and wakeups per second. Lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| CPU, ms per second | 5 | **2** | 5 | 25 |
| Wakeups per second | 98 | 99 | **50** | 3748 |

### Apple M1 (8 cores, 8 GB RAM)

| Workload | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 7.68 s | **11.19 s** (69%) | 11.54 s (67%) | 23.58 s (33%) | 25.85 s (30%) |
| `pnpm install` | 4.47 s | **5.53 s** (81%) | 5.89 s (76%) | — | 46.36 s (10%) |
| `yarn install` | 9.62 s | 10.54 s (91%) | **10.36 s** (93%) | 28.10 s (34%) | 35.38 s (27%) |
| `ripgrep` (file read) | 1.21 s | **193 ms** (627%) | 1.11 s (109%) | 17.04 s (7%) | 13.24 s (9%) |
| `find` (metadata walk) | 512 ms | **121 ms** (423%) | 552 ms (93%) | 3.81 s (13%) | 4.10 s (12%) |
| `cp -a node_modules` | 21.50 s | **5.32 s** (404%) | 15.19 s (142%) | 62.92 s (34%) | 44.64 s (48%) |
| `rm -rf node_modules` | 5.37 s | **3.46 s** (155%) | 3.97 s (135%) | 12.38 s (43%) | 12.78 s (42%) |
| Host file edit -> container | 2 ms | **2 ms** | **2 ms** | 6 ms | 10 ms |

| Workload (own disk) | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| `npm ci` | **7.96 s** | 10.39 s | 11.30 s | 12.60 s |
| `pnpm install` | 2.04 s | 2.44 s | **1.56 s** | 2.23 s |
| `yarn install` | **7.84 s** | 7.88 s | 11.55 s | 11.74 s |
| `ripgrep` (file read) | **132 ms** | 160 ms | 183 ms | 260 ms |
| `find` (metadata walk) | **128 ms** | 132 ms | 215 ms | 152 ms |
| `cp -a node_modules` | **3.12 s** | 3.21 s | 3.26 s | 6.20 s |
| `rm -rf node_modules` | 602 ms | 736 ms | 729 ms | **592 ms** |

#### What the runtime costs the Mac

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
| TCP, container to the Mac | Mbit/s | 118995 | 52498 | **63579** | 4311 | 13592 |
| TCP, the Mac to a container | Mbit/s | 118041 | **42745** | 30343 | 3185 | 10426 |
| TCP into a published port | Mbit/s | — | **43174** | 29949 | 3048 | 10292 |
| TCP out of a published port | Mbit/s | — | 54282 | **67470** | 3791 | 22382 |
| UDP, container to the Mac | Mbit/s | 24503 | **4858** | 3139 | 2612 | 0 |
| connects to a published port | per second | 25453 | 13413 | **16477** | 9077 | 16221 |
| GET on a published port, median | µs | 54 | 154 | **128** | 464 | 191 |
| GET on a published port, p99 | µs | 97 | 245 | **231** | 539 | 497 |
| DNS lookup from a container, median | µs | 3851 | **153** | 422 | 714 | 765 |

#### What an idle runtime costs

After a quiet minute, a minute of powermetrics samples over the runtime's processes: CPU as milliseconds of core per second, and wakeups per second. Lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| CPU, ms per second | 24 | 21 | **12** | 42 |
| Wakeups per second | 329 | 91 | **59** | 2011 |

`benchmarks/RESULTS.md` contains the full logs, individual repetition timings, and methodology.
## Why it is fast

The performance of containers on macOS comes down to three bottlenecks: the shared filesystem, virtual disk I/O, and memory management.

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

---

## What it does

- **Docker and Compose compatibility:** Full support via standard Docker CLI and Compose plugins.
- **Bidirectional port forwarding:** Exposed ports appear on `localhost` instantly via a low-overhead network helper (`gvproxy`).
- **Native file sharing:** Mount any directory from your Mac with native ownership translation.
- **x86-64 container emulation:** Run `linux/amd64` images under software emulation (`qemu-user`).
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
lighter (CLI)  ──spawns──▶  lighter run  ──────────▶  gvproxy (network sidecar)
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
