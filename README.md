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

Measured on clean machines against a 1,232-package `package.json` fixture (`benchmarks/`). Each figure is the median of three timed repetitions, following an untimed warm-up run. Numbers are reported both in wall-clock time and as a fraction of running the exact same command natively on the Mac's own APFS disk.

OrbStack was measured on the same machines in the same sessions, not quoted from marketing materials.

### Apple M5 Pro (18 cores, 48 GB RAM)

| Workload | native APFS | lighter (share) | OrbStack (share) | lighter (own disk) | OrbStack (own disk) |
|---|---|---|---|---|---|
| `npm ci` | 6.64 s | **6.31 s** (105%) | 9.09 s (73%) | **4.78 s** (139%) | 7.07 s (94%) |
| `pnpm install` | 4.61 s | **4.72 s** (98%) | 5.54 s (83%) | **1.21 s** (381%) | 2.04 s (226%) |
| `yarn install` | 5.67 s | **5.38 s** (105%) | 8.38 s (68%) | **4.27 s** (133%) | 5.27 s (108%) |
| `ripgrep` (file read) | 909 ms | **84 ms** (1082%) | 1047 ms (87%) | **91 ms** (999%) | 112 ms (812%) |
| `find` (metadata walk) | 386 ms | **96 ms** (402%) | 519 ms (74%) | **97 ms** (398%) | 128 ms (302%) |
| `cp -a node_modules` | 14.55 s | **3.31 s** (439%) | 9.40 s (155%) | **0.96 s** (1524%) | 1.15 s (1265%) |
| `rm -rf node_modules` | 4.19 s | **2.58 s** (162%) | 3.38 s (124%) | **386 ms** (1084%) | 479 ms (874%) |
| Host file edit -> container | 2 ms | **3 ms** | 11 ms | n/a | n/a |

### Apple M1 (8 cores, 8 GB RAM)

On an 8 GB machine under memory pressure, lighter matches OrbStack on package manager wall times while running traversal, search, and bulk operations substantially faster:

| Workload | native APFS | lighter (share) | OrbStack (share) | lighter (own disk) | OrbStack (own disk) |
|---|---|---|---|---|---|
| `pnpm install` | 4.65 s | **6.02 s** (77%) | 6.06 s (77%) | **1.76 s** (264%) | 2.38 s (196%) |
| `npm ci` | 7.85 s | 11.66 s (67%) | **11.09 s** (71%) | **7.87 s** (100%) | 10.21 s (77%) |
| `yarn install` | 10.62 s | 12.19 s (87%) | **10.23 s** (104%) | **8.02 s** (132%) | 7.82 s (136%) |
| `ripgrep` (file read) | 1330 ms | **151 ms** (881%) | 1121 ms (119%) | **129 ms** (1031%) | 184 ms (723%) |
| `find` (metadata walk) | 503 ms | **122 ms** (412%) | 518 ms (97%) | **125 ms** (402%) | 130 ms (387%) |
| `cp -a node_modules` | 24.76 s | **4.96 s** (499%) | 16.57 s (149%) | **2.48 s** (999%) | 3.62 s (683%) |
| `rm -rf node_modules` | 5.43 s | **3.53 s** (154%) | 4.02 s (135%) | **590 ms** (921%) | 695 ms (782%) |
| Host file edit -> container | 2 ms | **2 ms** | 2 ms | n/a | n/a |

`benchmarks/RESULTS.md` contains the full logs, individual repetition timings, and methodology.

---

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
