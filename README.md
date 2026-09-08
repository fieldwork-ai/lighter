# lighter

**The open-source, headless container engine for macOS.**

lighter is a lightweight virtual machine monitor built from scratch on Apple's `Hypervisor.framework` in Rust. It implements its own vCPU loop, GICv3 interrupt controller, bespoke virtio device models, and boots a custom Linux LTS kernel directly into memory in 50 milliseconds.

Purpose-built for Apple Silicon, lighter is a drop-in replacement for Docker Desktop and OrbStack. It matches or beats OrbStack's speed, consumes a fraction of Docker Desktop's memory, runs completely headless with zero GUI bloat, and comes with zero commercial licensing traps.

**Dual-licensed MIT or Apache 2.0. No paid subscriptions, no commercial seat limits, no "free during beta", and no telemetry.**

*Requires Apple Silicon (M1–M5) and macOS 15 (Sequoia) or later.*

---

## At a glance

| Metric / Feature | lighter | OrbStack | Docker Desktop | Colima |
|---|---|---|---|---|
| **License** | **MIT / Apache 2.0** | Proprietary | Proprietary | Apache 2.0 |
| **Commercial use** | **Free forever** | $8–$10 / user / mo | $9–$24 / user / mo (≥250) | Free |
| **Telemetry** | **Zero** | Yes | Yes | None |
| **GUI overhead** | **None (Headless)** | Menu bar / App | Electron app | None (Lima) |
| **Cold start (to container)** | **664 ms** | 1.4 s | 2.1 s | 9.0 s |
| **Idle memory** | **372 MiB** | 936 MiB | 3,493 MiB | 1,302 MiB |
| **Memory 15s after heavy build** | **702 MiB** | 2,776 MiB | — | 10,145 MiB |
| **`npm ci` (own disk)** | **4.49 s** | 6.83 s | 7.96 s | 7.56 s |
| **`npm ci` (host share)** | **6.36 s** | 8.53 s | — | 17.89 s |
| **Host share copy (`cp -a`)** | **3.80 s** | 9.58 s | — | 41.95 s |
| **Container DNS resolution** | **40 µs** | 262 µs | 513 µs | 481 µs |
| **Kubernetes support** | **kind, kubectl, Helm** | Built-in | Built-in | k3s |
| **x86-64 Rosetta (`sha256sum`)** | **4.16 s** | 7.92 s | 4.39 s | 4.26 s |

---

## Install

### One-line installer

```bash
curl -fsSL https://raw.githubusercontent.com/fieldwork-ai/lighter/main/scripts/install.sh | sh
```

### Or via Homebrew

```bash
brew tap fieldwork-ai/tap
brew install lighter
```

### Quick start

Start the background daemon:

```bash
lighter start
```

`lighter start` boots the VM and configures a standard Docker CLI context. Your existing `docker` and `docker compose` commands work immediately, with nothing to export and no manual socket flags:

```bash
docker run --rm alpine echo "hello from lighter"
```

### Common commands

```bash
lighter status      # VM state, vCPU count, memory footprint, and disk usage
lighter doctor      # Verify macOS hypervisor entitlements and configuration
lighter config      # View or change CPU, memory, and disk allocations
lighter install     # Register with launchd to start automatically on login
lighter stop        # Cleanly shut down the machine in ~500 ms
lighter upgrade     # Upgrade to the latest release
```

Direct installations can opt into background update downloads with `lighter update auto-download on` (downloads never activate without an explicit restart). Homebrew installations remain managed by `brew`. See [installation ownership, migration, and update behaviour](docs/updates.md).

---

## Benchmarks

All benchmarks are measured against identical pinned workloads on Apple Silicon. Higher percentages of native APFS mean faster; **bold** indicates the best runtime result.

On Apple Silicon, lighter launches containers cold in **664 ms** (over 2x faster than OrbStack), runs `npm ci` on host shares in **6.36 s** (faster than native APFS, beating OrbStack's 8.53 s), completes directory copies **2.5x faster**, idles at **372 MiB RAM**, and returns memory to macOS within seconds of a workload finishing.

<details>
<summary>Benchmark methodology & test environment</summary>

Measured with the pinned 1,232-package fixture in `benchmarks/` on a MacBook Pro (Apple M5 Pro, 18 cores, 48 GB RAM, macOS 15 Sequoia). Timing rows report medians of three measured repetitions. Native and container runs use identical pinned Node, npm, pnpm, and Yarn versions. All runtimes were configured with 8 vCPUs and 16 GiB RAM allocations where supported. Docker Desktop is measured using Virtualization.framework, VirtioFS, and Rosetta.

Lighter measurements reflect the 0.5.1 release; competitor measurements retain their 0.5.0-release suite. Docker Desktop's host-share cleanup failed during testing; affected install timings and dependent storage memory results are excluded. Raw observations, environment fingerprints, and full M1 results are preserved in [the 0.5.1 measurements](benchmarks/RELEASE-0.5.1.md), [the retained 0.5.0 comparison](benchmarks/RELEASE-0.5.0.md), and [benchmarks/RESULTS.md](benchmarks/RESULTS.md). See [repeatability](benchmarks/REPEATABILITY.md) for workload-specific variation.
</details>

### MacBook Pro — Apple M5 Pro (18 cores, 48 GB RAM)

| Workload (own disk) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 6.53 s | **4.49 s** (145%) | 6.83 s (96%) | 7.56 s (86%) | 7.96 s (82%) |
| `pnpm install` | 4.32 s | 1.22 s (354%) | 1.80 s (240%) | **1.06 s** (408%) | 2.75 s (157%) |
| `yarn install` | 5.89 s | **4.10 s** (144%) | 5.05 s (117%) | 6.00 s (98%) | 10.10 s (58%) |
| `ripgrep` (file read) | 927 ms | **85 ms** (1091%) | 118 ms (786%) | 115 ms (806%) | 126 ms (736%) |
| `find` (metadata walk) | 390 ms | **95 ms** (411%) | 129 ms (302%) | 185 ms (211%) | 124 ms (315%) |
| `cp -a node_modules` | 16.76 s | **898 ms** (1866%) | 1.05 s (1599%) | 1.35 s (1242%) | 2.49 s (673%) |
| `rm -rf node_modules` | 4.18 s | **396 ms** (1056%) | 473 ms (884%) | 488 ms (857%) | 397 ms (1053%) |

| Workload (host share) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 6.53 s | **6.36 s** (103%) | 8.53 s (77%) | 17.89 s (36%) | — |
| `pnpm install` | 4.32 s | **4.01 s** (108%) | 4.93 s (88%) | 25.95 s (17%) | — |
| `yarn install` | 5.89 s | **5.19 s** (113%) | 7.97 s (74%) | 22.81 s (26%) | — |
| `ripgrep` (file read) | 927 ms | **88 ms** (1053%) | 1.00 s (93%) | 3.02 s (31%) | — |
| `find` (metadata walk) | 390 ms | **92 ms** (424%) | 452 ms (86%) | 1.46 s (27%) | — |
| `cp -a node_modules` | 16.76 s | **3.80 s** (441%) | 9.58 s (175%) | 41.95 s (40%) | — |
| `rm -rf node_modules` | 4.18 s | **3.22 s** (130%) | 3.35 s (125%) | 8.38 s (50%) | — |
| Host file edit -> container | 1 ms | **2 ms** | 11 ms | **2 ms** | 3 ms |

#### Memory footprint

macOS physical footprint (Activity Monitor "Memory") for runtime processes: idle after cold start, peak during `npm ci`, and 15s / 60s after workload completion. Lower is better. lighter releases memory back to the Mac immediately via `virtio-mem` and cooperative reclamation.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| Idle, a minute after start | **372 MiB** | 936 MiB | 1302 MiB | 3493 MiB |
| Peak through an npm install | **3841 MiB** | 5699 MiB | 10054 MiB | — |
| 15 s after it ends | **702 MiB** | 2776 MiB | 10145 MiB | — |
| 60 s after it ends | **726 MiB** | 1720 MiB | 10149 MiB | — |

#### The network

Throughput and latency between container and host measured with `iperf3`, keep-alive HTTP GET latency, connection setup rate, and container DNS resolution time. Bold marks best result.

| Case | unit | native | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|---|
| TCP, container to the Mac | Gbit/s | 124.1 | 91.0 | **95.1** | 4.5 | 24.5 |
| TCP, the Mac to a container | Gbit/s | 131.2 | **83.7** | 51.5 | 4.0 | 14.7 |
| TCP into a published port | Gbit/s | — | **87.0** | 52.3 | 3.9 | 14.5 |
| TCP out of a published port | Gbit/s | — | **92.0** | 88.3 | 4.2 | 32.5 |
| UDP, container to the Mac | Gbit/s | 21.1 | **5.1** | 3.0 | 3.1 | 0.0 |
| connects to a published port | thousand per second | 26.6 | 17.1 | **21.2** | 16.1 | 15.7 |
| GET on a published port, median | µs | 40 | **64** | 74 | 229 | 125 |
| GET on a published port, p99 | µs | 68 | 213 | **132** | 366 | 232 |
| DNS lookup from a container, median | µs | 5611 | **40** | 262 | 481 | 513 |

#### Idle power

Idle CPU consumption and thread wakeups measured via `powermetrics` over a 60-second quiet window. Lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| CPU, ms per second | 3 | **2** | 4 | 36 |
| Wakeups per second | 56 | 120 | **41** | 3873 |

#### Starting up

Time from cold invocation (`lighter start`, `orb start`, `colima start`, Docker Desktop launch) until Docker engine responds, and until the first container completes. Median of three; lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| Start until docker answers | **517 ms** | 1.14 s | 8.80 s | 1.83 s |
| Start until the first container has run | **664 ms** | 1.41 s | 9.04 s | 2.11 s |

#### x86-64 images

Running `linux/amd64` images on Apple Silicon via Apple Rosetta (`--vz-rosetta` for Colima). Lower is better.

| Workload (x86-64 image, own disk) | lighter, arm64 | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 4.49 s | **9.14 s** | 13.14 s | 12.73 s | 14.28 s |
| `pnpm install` | 1.22 s | **2.75 s** | 3.58 s | 2.85 s | 3.85 s |
| `sha256sum` of 1 GiB | 3.02 s | **4.16 s** | 7.92 s | 4.26 s | 4.39 s |
| container start, `alpine true` | 138 ms | **156 ms** | 244 ms | 185 ms | 170 ms |

[0.5.1 release records](docs/records/0.5.1/hybrid/) and [retained competitor records](benchmarks/records/0.5.0/) retain raw CSVs, case diagnostics, selection decisions and environment evidence. `benchmarks/RESULTS.md` contains individual repetition timings and methodology.

---

## Why it is fast

Running containers on macOS typically hits five performance bottlenecks: the shared filesystem boundary, virtual disk I/O, guest memory hoarding, network packet translation, and cold-start latency. lighter solves each at the hypervisor level.

### 1. Shared filesystems without the boundary tax (`lighter-fs`)
Bind mounts on macOS are notoriously slow because every filesystem call crosses the hypervisor into APFS, where traversing tens of thousands of files incurs synchronous latency.

lighter eliminates the boundary overhead:
- **In-memory cache with host change notification:** The guest's page cache serves reads directly from memory without crossing the VM boundary. Host filesystem changes invalidate guest cache entries in real time via macOS `FSEvents`. If macOS event queues drop details under extreme load, a negotiated lease reset safely expires cached entries. Read latency drops to microsecond speeds—running `ripgrep` across a 1,232-package tree takes **91 ms**, compared to 1,000 ms on OrbStack and 3,020 ms on Colima.
- **Asynchronous mutation lanes:** Creates, writes, and renames complete in the guest immediately and flush to APFS via dedicated asynchronous worker queues.
- **Identity-based inode tracking:** When descriptor limits are reached under massive directory trees (e.g. 100,000+ files in `node_modules`), inodes are parked and referenced through parent directory descriptors by identity, avoiding path walks and descriptor churn.

### 2. Fast container storage (`btrfs` with reflinks)
Container writable layers and named volumes live on an internal virtual disk (`~/.lighter/data.img`) formatted as `btrfs` with `nodatacow` and single metadata:
- **Instant clones:** File copies (`cp -a` or `yarn` cache links) use `copy_file_range` to reflink extents without copying physical bytes on disk.
- **Inline interrupt completions:** A custom kernel patch allows checksum-free reads and writes on `nodatacow` volumes to complete directly inside the interrupt context rather than bouncing to worker threads.
- **Automatic reclamation:** Unused blocks are trimmed periodically and punched out of the host sparse image via `F_PUNCHHOLE`.

### 3. Cooperative memory management
Virtual machines that hoard allocated RAM starve macOS and trigger disk swapping.
- **Dynamic sizing with `virtio-mem`:** The guest boots from a quarter-sized base and onlines additional memory in 128 MiB blocks via `virtio-mem` as containers demand it. Host backing is prepared concurrently in the background; unused blocks and pages are returned to macOS.
- **Free page reporting:** `CONFIG_PAGE_REPORTING` surrenders unused guest pages directly to the host. Idle memory drops to **372 MiB** (compared to OrbStack's 936 MiB and Docker Desktop's 3,493 MiB). Within 15 seconds of completing a heavy build, lighter returns physical RAM to the host, resting at **702 MiB** while OrbStack holds 2,776 MiB and Colima holds 10,145 MiB.
- **Compressor-steered ballooning:** On memory-constrained Macs, macOS compresses memory before signaling out-of-memory pressure. lighter tracks host memory compression activity: when macOS begins compressing heavily, lighter's balloon inflates in aligned 16 KiB blocks to yield host physical memory, deflating once compression subsides.

### 4. The network as streams, not packets
Other runtimes assign the VM a virtual network interface card and run a userspace TCP/IP stack on the Mac to translate raw packets back into host connections. Every byte is copied and checksummed twice, with round-trip hypervisor context switches on every packet.

lighter avoids packet transport across the VM boundary entirely:
- **Direct stream bridging:** When a container opens a TCP connection, the guest kernel redirects it to lighter's agent, which establishes a single vsock stream to the host. The host opens a native macOS socket to the destination and copies bytes between the two. The Mac's native network stack handles routing, VPNs, and proxies automatically.
- **In-kernel BPF sockmap:** The container socket and the vsock stream are joined directly in the guest kernel via a BPF sockmap. The data path is a zero-process kernel-to-kernel copy.
- **Native host DNS resolution:** Container DNS queries are resolved directly by the macOS host resolver. Lookup latency drops to **40 µs**—over six times faster than OrbStack (262 µs) and nearly thirteen times faster than Docker Desktop (513 µs).
- **Low-latency polling:** After every network event, the host transport thread polls briefly before sleeping, servicing immediate request-response replies without scheduler wake latency.

### 5. Sub-second startup (664 ms cold start)
Cold start includes allocating VM metadata, booting Linux, and initializing Docker. At 16 GiB on M5, Docker answers in **517 ms** and completes the first container in **664 ms**—more than twice as fast as OrbStack:
- **Hybrid RAM preparation:** lighter boots the guest immediately while memory backing is prepared concurrently in the background. First access safely prepares pages ahead of the worker, eliminating startup pauses without forfeiting whole-range reclamation.
- **50-millisecond custom kernel boot:** Hardware probing is stripped down strictly to the virtual devices present.
- **Parallel containerd initialization:** Init launches `containerd` immediately upon disk mount and attaches `dockerd` without polling delays.
- **Optimized disk flushes:** Guest disk flushes map to drive-level image `fsync`, avoiding macOS drive-cache commit penalties that add 4 ms per flush.
- **Expedited RCU grace periods:** Container network namespace creation and teardown leverage expedited RCU scans without waiting on scheduler timer ticks.

### 6. Minimal Linux LTS kernel strategy
lighter runs an official Longterm Support kernel (`6.18-lighter`) with a minimal, audited patch set focused strictly on hypervisor performance:
- `virtio-mem` independent block page arrays and auto-movable onlining (`0024`, `0026`).
- BPF sockmap backoff to avoid backlog worker spinning (`0025`).
- Apple Silicon TSO memory ordering for high-speed Rosetta x86-64 execution (`0023`).
- `btrfs` direct interrupt-context completions (`0009`).
- Adaptive idle polling before WFI to eliminate cross-vCPU IPI latency during parallel builds (`0011`).

Kernel releases track upstream Linux LTS point updates, ensuring ongoing security patches and driver fixes without architectural churn.

---

## Features

- **Docker CLI & Compose compatibility:** Works seamlessly as a registered Docker context with existing `docker`, `docker compose`, and third-party developer tooling.
- **x86-64 containers under Rosetta:** Run `linux/amd64` images on Apple Silicon with near-native performance via Apple Rosetta (`lighter rosetta --install`). See [x86-64 architecture and performance](docs/x86-64.md).
- **Local Kubernetes with kind:** Spin up single-node and multi-node arm64 Kubernetes clusters with standard `kind`, `kubectl`, and `helm` commands without control-plane overhead when idle. See the [Kubernetes guide](docs/kubernetes.md).
- **Bidirectional port forwarding & IPv6:** Published ports (`-p 8080:80` or `-p 127.0.0.1:8080:80`) bind directly on the Mac. Full IPv6 routing is supported whenever the host network supports it.
- **Native file sharing:** Mount host directories into containers with automatic UID/GID ownership translation and real-time cache synchronization.
- **Headless background operation:** Runs as a lean terminal daemon or background `launchd` service with zero menu bar clutter and virtually zero idle CPU usage (~0.2%).

---

## Out of scope

- **GUI apps:** lighter is headless by design. We do not build Electron apps, menu bar dashboards, or system tray widgets.
- **Managed Kubernetes engine:** Local clusters run through standard tools like [kind](docs/kubernetes.md); lighter does not run an unmanaged Kubernetes control plane.
- **Intel Macs:** Purpose-built exclusively for Apple Silicon (ARM64).
- **Windows or Linux hosts:** Purpose-built exclusively for macOS.

---

## Architecture

The codebase is split into discrete Rust crates, each with a single responsibility:

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

# 2. Build lighter CLI and VMM, ad-hoc signed with hypervisor entitlement
make build

# 3. Run milestone verification gates
make gates
```

Milestone gates (`make gates`) boot real test VMs to verify end-to-end functionality: kernel boot, device negotiation, Docker engine readiness, network egress, shared filesystem coherency, and memory reclamation.

---

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
