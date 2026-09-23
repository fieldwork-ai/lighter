# 🔥 lighter

**The fast, open-source container engine for macOS, with native Apple Silicon GPU, Neural Engine, and PyTorch acceleration.**

<a href="https://getfieldwork.ai">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/fieldwork-logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/fieldwork-logo-light.svg">
    <img alt="Fieldwork" src="assets/fieldwork-logo-light.svg" height="24">
  </picture>
</a>

*lighter is an open-source project sponsored by [Fieldwork](https://getfieldwork.ai), providing dedicated engineering time to build and maintain high-performance virtualization and AI infrastructure for Apple Silicon.*

lighter is a high-performance, headless virtual machine monitor built from scratch in Rust on Apple's `Hypervisor.framework`. It boots a custom Linux LTS kernel directly into memory in 50 milliseconds, delivers shared filesystem performance faster than native APFS, and is the **first and only macOS container engine to give Linux containers direct access to Apple Silicon GPU, Metal, Neural Engine, and hardware media encode/decode**.

A seamless, drop-in replacement for Docker Desktop, OrbStack, and Colima:
- 🔄 **Drop-in Docker replacement:** Works immediately with your existing `docker`, `docker compose`, `kind`, and third-party developer tooling.
- 🚀 **Full Apple Silicon acceleration:** Run local LLMs, PyTorch (MPS), computer vision models and video decode and encode directly on your Mac's GPU, Neural Engine and media engine.
- ⚡ **Blistering performance:** Cold boots in 709 ms; host file mounts and builds run faster than native APFS.
- 🪶 **Ultra-lightweight:** Idles at 617 MiB RAM (vs 3.5 GB for Docker Desktop) and surrenders memory back to macOS within seconds of a workload finishing.
- 🆓 **100% Free & Open Source:** Dual-licensed MIT / Apache 2.0. No paid subscriptions, no commercial seat licenses, no telemetry, and zero GUI/Electron bloat.

*Requires Apple Silicon and macOS 15+ (Sequoia, Tahoe).*

---

## At a glance

| Metric / Feature | lighter | OrbStack | Docker Desktop | Colima |
|---|---|---|---|---|
| **License** | **MIT / Apache 2.0** | Proprietary | Proprietary | Apache 2.0 |
| **Commercial use** | **Free forever** | $8–$10 / user / mo | $9–$24 / user / mo (≥250) | Free |
| **Telemetry** | **Zero** | Yes | Yes | None |
| **GUI overhead** | **None (Headless)** | Menu bar / App | Electron app | None (Lima) |
| **Cold start (to container)** | **709 ms** | 1.4 s | 2.1 s | 9.0 s |
| **Idle memory** | **617 MiB** | 936 MiB | 3,493 MiB | 1,302 MiB |
| **Memory 15s after heavy build** | **726 MiB** | 2,776 MiB | 7,276 MiB | 10,145 MiB |
| **`npm ci` (own disk)** | **4.41 s** | 6.83 s | 7.96 s | 7.56 s |
| **`npm ci` (host share)** | **6.72 s** | 8.53 s | N/A | 17.89 s |
| **Host share copy (`cp -a`)** | **3.67 s** | 9.58 s | N/A | 41.95 s |
| **Container DNS resolution** | **39 µs** | 262 µs | 513 µs | 481 µs |
| **Kubernetes support** | **kind, kubectl, Helm** | Built-in | Built-in | k3s |
| **x86-64 Rosetta (`sha256sum`)** | **4.11 s** | 7.92 s | 4.39 s | 4.26 s |
| **Apple Silicon GPU (Vulkan)** | **Yes** | No | No | No |
| **Apple Neural Engine (ANE)** | **Yes** | No | No | No |
| **PyTorch on Mac GPU (MPS)** | **Yes** | No | No | No |
| **llama.cpp / whisper on Metal** | **Yes** (93 t/s on M1, 299 on M5) | No | No | No |
| **Hardware video decode and encode (H.264, HEVC, VP9)** | **Yes** (HEVC 1080p30 encode at 21% of a core; libx265 cannot keep up) | No | No | No |

---

## Why switch to lighter?

- 🚫 **Escape Docker Desktop's bloat & licensing fees:** Docker Desktop consumes 3.5–7+ GB of RAM, runs Electron in the background, spins laptop fans, and charges $9–$24/user/month for commercial teams. lighter is a lean terminal daemon using ~600 MiB RAM at idle (~350 MiB on an 8 GB Mac), with zero licensing costs forever.
- 🔓 **Free & Open Source forever:** OrbStack transitioned to a closed-source, paid subscription model ($8–$10/user/month). lighter is dual-licensed MIT / Apache 2.0 with zero commercial seat limits, no "free during beta" bait-and-switch, and zero telemetry.
- 🧠 **Unlock Apple Silicon AI & GPU acceleration:** Docker Desktop, OrbStack, and Colima offer *zero* Apple Silicon hardware acceleration. lighter gives your containers native Metal (93 t/s on M1, 299 t/s on M5), PyTorch MPS training, Neural Engine inference at <1% CPU, and full H.264/HEVC/VP9 hardware video decode and encode.
- ⚡ **Shared folders faster than native macOS:** Bind-mounting code into containers on macOS is historically painful. `lighter-fs` uses an in-memory page cache with real-time `FSEvents` invalidation, making `npm ci` and `ripgrep` faster inside containers than native APFS.
- 🔌 **100% Drop-in Docker compatibility:** Zero workflow changes. `lighter start` sets up your Docker CLI context. Run existing `docker`, `docker compose`, `kind`, and CI scripts as normal.

---

## Install

### Via Homebrew (recommended)

```bash
brew tap fieldwork-ai/tap
brew install lighter
```

### Or via one-line installer

```bash
curl -fsSL https://raw.githubusercontent.com/fieldwork-ai/lighter/main/scripts/install.sh | sh
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

## Hardware & AI Acceleration

lighter is the first container runtime for macOS to put the Neural Engine and PyTorch's `mps` device inside Linux containers, it runs llama.cpp and whisper.cpp on the Mac's GPU with ggml's own Metal kernels, and it decodes and encodes video on the Mac's media engine for anything that speaks V4L2. Vulkan in containers follows the libkrun design that Podman's krunkit has shipped since 2024: a virtio-gpu Venus device rendered over MoltenVK. All five devices use Docker's standard Container Device Interface (CDI) via `--device` and need no flags to enable.

| Device | What a container gets | Measured performance |
|---|---|---|
| `lighter.sh/gpu` | Vulkan on the Mac's GPU through Venus | llama.cpp 45 t/s on M1 |
| `lighter.sh/metal` | ggml's Metal kernels over RPC | 93 t/s on M1 (85% of native); 299 on M5 |
| `lighter.sh/ane` | ONNX models on Neural Engine, GPU or CPU (fastest chosen) | ResNet-50 2.2 ms vs 29 ms on container CPU |
| `lighter.sh/mps` | PyTorch on the Mac's GPU | Training step 14 ms on M1, 7 ms on M5 |
| `lighter.sh/video` | V4L2 decoder (`/dev/video0`: H.264, HEVC, VP9) and encoder (`/dev/video1`: H.264, HEVC) on the Mac's media engine | H.264 4K30 decode at 16% of a core, 69% in software; H.264 1080p30 encode at 22%, 78% in libx264 (M1) |

Two documented costs (`docs/gpu.md`): cold start is ~50 ms longer with accelerator devices enabled (564 / 715 ms vs 517 / 664 ms on M5), and idle memory is ~20 MiB higher for the in-process servers' readiness. Devices can be disabled individually if desired (`lighter config --gpu off`, `--ane off`, `--mps off`, `--metal off`, `--video off`).

Persistent background services (such as Frigate NVR or continuous speech recognition) do not waste host CPU while waiting for work. Adaptive idle polling backs off whenever vCPUs are not actively catching events, allowing Frigate on a camera stream with Neural Engine detection to consume just 15.4% of a core on a 16-vCPU guest.

See [`docs/gpu.md`](docs/gpu.md) for complete technical documentation and architecture.

### 1. ggml & llama.cpp on Metal (`--device lighter.sh/metal=all`)

Run `llama.cpp`, `whisper.cpp`, and ggml-based models directly on Apple Silicon Metal kernels via an in-process RPC engine:

```bash
docker run --rm --device lighter.sh/metal=all -v ./models:/models llama-cpp-rpc \
  llama-bench -m /models/qwen2.5-0.5b-instruct-q4_k_m.gguf --rpc "$LIGHTER_METAL" -ngl 99
```

- **93 tokens/sec generation on M1** (85% of native 110 t/s; vs 21 t/s on CPU) and **299 tokens/sec on M5**.
- **1,911 prompt tokens/sec** in-container vs 1,953 native.
- **Whisper transcription**: 11-second audio clip transcribed in 1.30s (vs 5.80s on container CPU).
- Includes automatic host-side tensor caching (`~/.lighter/ggml-cache`) for instant reloads.

### 2. PyTorch with Apple Silicon MPS (`--device lighter.sh/mps=all`)

Linux PyTorch has no native Apple MPS backend. lighter provides `lighter-mps` inside the container, forwarding ATen operators across vsock to the host Mac's GPU:

```bash
docker run --rm --device lighter.sh/mps=all python:3.12-slim sh -c '
  pip install torch --index-url https://download.pytorch.org/whl/cpu && pip install lighter-mps &&
  python -c "import torch, lighter_mps; x = torch.randn(3, 3, device=\"mps\"); print((x @ x).device)"'
```

- Training step: **14 ms on M1, 7 ms on M5**.
- Native `model.to("mps")` works seamlessly for both inference and training with autograd.
- Host PyTorch is discovered automatically from your macOS environment (`lighter config --torch-python`).
- The wheels are built for CPython 3.11 to 3.13 against torch 2.14.0, and the host's torch must be the same version, with numpy beside it; `lighter doctor` says which it found. A 3.14 image or another torch fails at `pip install lighter-mps` or at start.

### 3. Apple Neural Engine (`--device lighter.sh/ane=all`)

Execute computer vision and edge inference on Apple's Neural Engine at a fraction of a watt:

```python
import onnxruntime as ort

ort.register_execution_provider_library("lighter", "/usr/lib/lighter/liblighter_ane_ep.so")
devices = [d for d in ort.get_ep_devices() if d.ep_name == "LighterANE"]
options = ort.SessionOptions()
options.add_provider_for_devices(devices, {})
session = ort.InferenceSession("model.onnx", options)
```

- Ships a custom `no_std` ONNX Runtime Execution Provider (`liblighter_ane_ep.so`) linking no libc.
- **ResNet-50 in 2.2 ms** on the Neural Engine (vs 29 ms on container CPU).
- **Frigate NVR**: YOLO11n object detection runs at **7.6 ms/frame at <1% CPU** (vs 15.1 ms and 36% CPU on container CPU).
- Automatic tiering: Model loads across Neural Engine, GPU, and CPU paths on first run; fastest candidate is automatically chosen and CoreML compiled models are cached in `coreml-cache`.

### 4. Hardware video decode and encode (`--device lighter.sh/video=all`)

A container gets `/dev/video0`, a V4L2 stateful decoder (H.264, HEVC in 8 and 10 bits, VP9), and `/dev/video1`, the matching encoder (H.264, HEVC Main and Main 10), of the kind a Raspberry Pi has, backed by VideoToolbox. Stock ffmpeg, GStreamer, Jellyfin, go2rtc and Frigate's own ffmpeg use them unchanged:

```bash
docker run --rm --device lighter.sh/video=all -v "$PWD:/w" debian:bookworm-slim sh -c \
  'apt-get update -qq && apt-get install -y -qq ffmpeg >/dev/null &&
   ffmpeg -c:v h264_v4l2m2m -i /w/input.mkv -c:v hevc_v4l2m2m -b:v 6M /w/output.mp4'
```

| In real time, 30 fps (M1) | Software | V4L2 | Native VideoToolbox |
|---|---|---|---|
| Decode H.264 4K, 25 Mbps | 69% of a core | **16%** | 12% |
| Decode HEVC Main 10 4K, 20 Mbps | 100% | **30%** | 15% |
| Decode VP9 1080p, 6 Mbps | 44% | **9%** | 6% |
| Encode H.264 1080p, 8 Mbps | 78% (libx264 veryfast) | **22%** | 9% |
| Encode HEVC 1080p, 6 Mbps | cannot keep up (libx265 ultrafast) | **21%** | 9% |
| Encode HEVC 4K, 20 Mbps | cannot keep up | **42%** | 26% |

- Eight-bit decode is identical to software decode, frame for frame; encode is verified for bit-exact keyframe placement, bitrate target adherence, and PSNR quality against the source.
- **Frigate NVR**: `ffmpeg.hwaccel_args: -c:v h264_v4l2m2m` (or `hevc_v4l2m2m` for an H.265 camera) on the camera. Detecting on a 5 MP main stream: 32% of a core to 22%.
- **Jellyfin**: V4L2 hardware acceleration transcodes on the encoder. **go2rtc**: `#hardware=v4l2m2m`.
- One copy a frame each way between the guest and VideoToolbox. No AV1. See [the guide](docs/gpu.md) for the details.

### 5. General-Purpose Vulkan (`--device lighter.sh/gpu=all`)

Exposes a virtio-gpu Venus render node decoded on macOS by `virglrenderer` over `MoltenVK`:

```bash
docker run --rm --device lighter.sh/gpu=all alpine:edge sh -c \
  'apk add mesa-vulkan-virtio vulkan-loader vulkan-tools && vulkaninfo --summary'
# Output: deviceName = Virtio-GPU Venus (Apple M...)
```

- Works out-of-the-box with standard distribution Mesa drivers (`mesa-vulkan-virtio`, `mesa-vulkan-drivers`).
- Accelerates Vulkan compute, ncnn, ONNX WebGPU, and graphics workloads.

---

## Benchmarks

All benchmarks are measured against identical pinned workloads on Apple Silicon. Higher percentages of native APFS mean faster; **bold** indicates the best runtime result.

On Apple Silicon, lighter launches containers cold in **709 ms** (over 2x faster than OrbStack), runs `npm ci` on host shares in **6.72 s** (faster than native APFS, beating OrbStack's 8.53 s), completes directory copies **2.6x faster**, idles at **617 MiB RAM**, and returns memory to macOS within seconds of a workload finishing.

<details>
<summary>Benchmark methodology & test environment</summary>

Measured with the pinned 1,232-package fixture in `benchmarks/` on a MacBook Pro (Apple M5 Pro, 18 cores, 48 GB RAM, macOS 26 Tahoe). Timing rows report medians of three measured repetitions. Native and container runs use identical pinned Node, npm, pnpm, and Yarn versions. All runtimes were configured with 8 vCPUs and 16 GiB RAM allocations where supported. Docker Desktop is measured using Virtualization.framework, VirtioFS, and Rosetta. Raw observations, environment fingerprints, and individual repetition timings are in `benchmarks/results/`; `python3 benchmarks/report.py` prints them.
</details>

### MacBook Pro: Apple M5 Pro (18 cores, 48 GB RAM)

| Workload (own disk) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 6.53 s | **4.41 s** (148%) | 6.83 s (96%) | 7.56 s (86%) | 7.96 s (82%) |
| `pnpm install` | 4.32 s | 1.12 s (385%) | 1.80 s (240%) | **1.06 s** (408%) | 2.75 s (157%) |
| `yarn install` | 5.89 s | **4.04 s** (146%) | 5.05 s (117%) | 6.00 s (98%) | 10.10 s (58%) |
| `ripgrep` (file read) | 927 ms | **84 ms** (1104%) | 118 ms (786%) | 115 ms (806%) | 126 ms (736%) |
| `find` (metadata walk) | 390 ms | **92 ms** (424%) | 129 ms (302%) | 185 ms (211%) | 124 ms (315%) |
| `cp -a node_modules` | 16.76 s | **818 ms** (2049%) | 1.05 s (1599%) | 1.35 s (1242%) | 2.49 s (673%) |
| `rm -rf node_modules` | 4.18 s | **361 ms** (1158%) | 473 ms (884%) | 488 ms (857%) | 397 ms (1053%) |

| Workload (host share) | native APFS | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 6.53 s | **6.72 s** (97%) | 8.53 s (77%) | 17.89 s (36%) | N/A |
| `pnpm install` | 4.32 s | **4.08 s** (106%) | 4.93 s (88%) | 25.95 s (17%) | N/A |
| `yarn install` | 5.89 s | **5.10 s** (116%) | 7.97 s (74%) | 22.81 s (26%) | N/A |
| `ripgrep` (file read) | 927 ms | **86 ms** (1078%) | 1.00 s (93%) | 3.02 s (31%) | N/A |
| `find` (metadata walk) | 390 ms | **92 ms** (424%) | 452 ms (86%) | 1.46 s (27%) | N/A |
| `cp -a node_modules` | 16.76 s | **3.67 s** (457%) | 9.58 s (175%) | 41.95 s (40%) | N/A |
| `rm -rf node_modules` | 4.18 s | **2.35 s** (178%) | 3.35 s (125%) | 8.38 s (50%) | N/A |
| Host file edit -> container | 1 ms | **2 ms** | 11 ms | **2 ms** | 3 ms |

#### Memory footprint

macOS physical footprint (Activity Monitor "Memory") for runtime processes: idle after cold start, peak during `npm ci`, and 15s / 60s after workload completion. Lower is better. lighter returns memory to the Mac within seconds through free page reporting, proactive cache reclamation, and a cooperative balloon.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| Idle, a minute after start | **617 MiB** | 936 MiB | 1302 MiB | 3493 MiB |
| Peak through an npm install | **2770 MiB** | 5699 MiB | 10054 MiB | 7339 MiB |
| 15 s after it ends | **726 MiB** | 2776 MiB | 10145 MiB | 7276 MiB |
| 60 s after it ends | **714 MiB** | 1720 MiB | 10149 MiB | 7276 MiB |

#### The network

Throughput and latency between container and host measured with `iperf3`, keep-alive HTTP GET latency, connection setup rate, and container DNS resolution time. Bold marks best result.

| Case | unit | native | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|---|
| TCP, container to the Mac | Gbit/s | 124.1 | **104.2** | 95.1 | 4.5 | 24.5 |
| TCP, the Mac to a container | Gbit/s | 131.2 | **92.7** | 51.5 | 4.0 | 14.7 |
| TCP into a published port | Gbit/s | N/A | **99.0** | 52.3 | 3.9 | 14.5 |
| TCP out of a published port | Gbit/s | N/A | **104.2** | 88.3 | 4.2 | 32.5 |
| UDP, container to the Mac | Gbit/s | 21.1 | **5.2** | 3.0 | 3.1 | 0.0 |
| connects to a published port | thousand per second | 26.6 | 18.2 | **21.2** | 16.1 | 15.7 |
| GET on a published port, median | µs | 40 | **64** | 74 | 229 | 125 |
| GET on a published port, p99 | µs | 68 | 133 | **132** | 366 | 232 |
| DNS lookup from a container, median | µs | 5611 | **39** | 262 | 481 | 513 |

#### Idle power

Idle CPU consumption and thread wakeups measured via `powermetrics` over a 60-second quiet window. Lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| CPU, ms per second | 4 | **2** | 4 | 36 |
| Wakeups per second | 66 | 120 | **41** | 3873 |

#### Starting up

Time from cold invocation (`lighter start`, `orb start`, `colima start`, Docker Desktop launch) until Docker engine responds, and until the first container completes. Median of three; lower is better.

| Reading | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| Start until docker answers | **557 ms** | 1.14 s | 8.80 s | 1.83 s |
| Start until the first container has run | **709 ms** | 1.41 s | 9.04 s | 2.11 s |

#### x86-64 images

Running `linux/amd64` images on Apple Silicon via Apple Rosetta (`--vz-rosetta` for Colima). Lower is better.

| Workload (x86-64 image, own disk) | lighter, arm64 | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|---|
| `npm ci` | 4.41 s | **9.23 s** | 13.14 s | 12.73 s | 14.28 s |
| `pnpm install` | 1.12 s | **2.71 s** | 3.58 s | 2.85 s | 3.85 s |
| `sha256sum` of 1 GiB | 2.99 s | **4.11 s** | 7.92 s | 4.26 s | 4.39 s |
| container start, `alpine true` | 142 ms | **152 ms** | 244 ms | 185 ms | 170 ms |

The selected CSVs, their `.tree` environment descriptions and the selection manifests are in `benchmarks/results/`; `python3 benchmarks/report.py` prints every repetition. Each release's record is a row in [the worklog](docs/worklog.md).

---

## Why it's fast

Running containers on macOS typically hits five performance bottlenecks: the shared filesystem boundary, virtual disk I/O, guest memory hoarding, network packet translation, and cold-start latency. lighter solves each at the hypervisor level.

### 1. Shared filesystems without the boundary tax (`lighter-fs`)
Bind mounts on macOS are notoriously slow because every filesystem call crosses the hypervisor into APFS, where traversing tens of thousands of files incurs synchronous latency.

lighter eliminates the boundary overhead:
- **In-memory cache with host change notification:** The guest's page cache serves reads directly from memory without crossing the VM boundary. Host filesystem changes invalidate guest cache entries in real time via macOS `FSEvents`. If macOS event queues drop details under extreme load, a negotiated lease reset safely expires cached entries. Read latency drops to microsecond speeds: running `ripgrep` across a 1,232-package tree takes **91 ms**, compared to 1,000 ms on OrbStack and 3,020 ms on Colima.
- **Asynchronous mutation lanes:** Creates, writes, and renames complete in the guest immediately and flush to APFS via dedicated asynchronous worker queues.
- **Identity-based inode tracking:** When descriptor limits are reached under massive directory trees (e.g. 100,000+ files in `node_modules`), inodes are parked and referenced through parent directory descriptors by identity, avoiding path walks and descriptor churn.

### 2. Fast container storage (`btrfs` with reflinks)
Container writable layers and named volumes live on an internal virtual disk (`~/.lighter/data.img`) formatted as `btrfs` with `nodatacow` and single metadata:
- **Instant clones:** File copies (`cp -a` or `yarn` cache links) use `copy_file_range` to reflink extents without copying physical bytes on disk.
- **Inline interrupt completions:** A custom kernel patch allows checksum-free reads and writes on `nodatacow` volumes to complete directly inside the interrupt context rather than bouncing to worker threads.
- **Automatic reclamation:** Unused blocks are trimmed periodically and punched out of the host sparse image via `F_PUNCHHOLE`.

### 3. Cooperative memory management
Virtual machines that hoard allocated RAM starve macOS and trigger disk swapping.
- **One memory zone, demand-backed:** The guest owns all of its configured RAM from boot as ordinary memory, and the host prepares backing only for pages the guest touches. No hot-plugged range, no movable half: nothing can cap what the guest's kernel has, which is what starved a 16 GiB guest under a 4 GiB balloon before 0.7.2.
- **Free page reporting:** `CONFIG_PAGE_REPORTING` surrenders unused guest pages directly to the host. Idle memory sits at **617 MiB** on a 16 GiB guest (compared to OrbStack's 936 MiB and Docker Desktop's 3,493 MiB); 1.56% of that is the page array for the guest's whole RAM, the price of one memory zone whose kernel can use all of it. Within 15 seconds of completing a heavy build, lighter returns physical RAM to the host, resting at **726 MiB** (and 714 MiB at 60s) while OrbStack holds 2,776 MiB, Docker Desktop holds 7,276 MiB, and Colima holds 10,145 MiB.
- **Idle cache goes back while containers run:** A daily stack is never idle, so nothing that waits for idleness ever fires. The guest agent runs a proactive reclaim loop of the kind Meta runs fleet-wide: every six seconds a sliver of the containers' file cache goes back through `memory.reclaim`, the kernel choosing the coldest pages, sized by the Mac's need and limited by the guest's own measured memory stall, so a working set that starts refaulting stops it within a period. A day of image builds no longer sits in the Mac's swap.
- **Compressor-steered ballooning in whole pageblocks:** On memory-constrained Macs, macOS compresses memory before signaling out-of-memory pressure. lighter tracks host memory compression activity and swap against RAM: when macOS begins compressing heavily, lighter first asks the guest to reclaim its coldest container cache, then inflates the balloon in compound units from a whole 2 MiB pageblock down to 16 KiB, movable and migratable, so what the guest keeps stays compactable; it deflates down a paced ramp once compression subsides or the guest reports memory stalls.

### 4. The network as streams, not packets
Other runtimes assign the VM a virtual network interface card and run a userspace TCP/IP stack on the Mac to translate raw packets back into host connections. Every byte is copied and checksummed twice, with round-trip hypervisor context switches on every packet.

lighter avoids packet transport across the VM boundary entirely:
- **Direct stream bridging:** When a container opens a TCP connection, the guest kernel redirects it to lighter's agent, which establishes a single vsock stream to the host. The host opens a native macOS socket to the destination and copies bytes between the two. The Mac's native network stack handles routing, VPNs, and proxies automatically.
- **In-kernel BPF sockmap:** The container socket and the vsock stream are joined directly in the guest kernel via a BPF sockmap. The data path is a zero-process kernel-to-kernel copy.
- **Native host DNS resolution:** Container DNS queries are resolved directly by the macOS host resolver. Lookup latency drops to **39 µs**, over six times faster than OrbStack (262 µs) and nearly thirteen times faster than Docker Desktop (513 µs).
- **Low-latency polling:** After every network event, the host transport thread polls briefly before sleeping, servicing immediate request-response replies without scheduler wake latency.

### 5. Sub-second startup (709 ms cold start)
Cold start includes allocating VM metadata, booting Linux, and initializing Docker. At 16 GiB on M5, Docker answers in **557 ms** and completes the first container in **709 ms**, more than twice as fast as OrbStack:
- **Hybrid RAM preparation:** lighter boots the guest immediately while memory backing is prepared concurrently in the background. First access safely prepares pages ahead of the worker, eliminating startup pauses without forfeiting whole-range reclamation.
- **50-millisecond custom kernel boot:** Hardware probing is stripped down strictly to the virtual devices present.
- **Parallel containerd initialization:** Init launches `containerd` immediately upon disk mount and attaches `dockerd` without polling delays.
- **Optimized disk flushes:** Guest disk flushes map to drive-level image `fsync`, avoiding macOS drive-cache commit penalties that add 4 ms per flush.
- **Expedited RCU grace periods:** Container network namespace creation and teardown leverage expedited RCU scans without waiting on scheduler timer ticks.

### 6. Minimal Linux LTS kernel strategy
lighter runs an official Longterm Support kernel (`6.18-lighter`) with a minimal, audited patch set focused strictly on hypervisor performance:
- Balloon units from a whole pageblock down to a host page, movable and migratable, and a 64 KiB default for free page reporting before the agent sets its own (`0014`, `0034`); a vsock packet the allocator refuses is sent shorter rather than dropped (`0033`).
- BPF sockmap backoff to avoid backlog worker spinning (`0025`).
- Apple Silicon TSO memory ordering for high-speed Rosetta x86-64 execution (`0023`).
- `btrfs` direct interrupt-context completions (`0009`).
- Adaptive idle polling before WFI to eliminate cross-vCPU IPI latency during parallel builds (`0011`).

Kernel releases track upstream Linux LTS point updates, ensuring ongoing security patches and driver fixes without architectural churn.

### 7. Apple Silicon hardware acceleration with a small idle tax
OrbStack, Docker Desktop and Colima leave the Mac's GPU and Neural Engine inaccessible from Linux containers; Podman's krunkit reaches the GPU through Vulkan alone, and nothing else reaches the Neural Engine or gives PyTorch its `mps` device.

lighter exposes the Apple Silicon compute architecture to containers:
- **In-process static linking:** `virglrenderer`, `MoltenVK`, ONNX Runtime CoreML, and ggml are linked directly into the single `lighter` binary. No background helper processes, no network daemons.
- **Minimal idle footprint:** Accelerators initialise strictly on demand. The GPU renderer uses ~10 MB and two threads at boot; the ANE, MPS, and Metal servers cost nothing until invoked. Idle memory sits at 617 MiB (+20 MiB over the same guest without the servers).
- **Unified memory apertures:** Guest GPU blobs are mapped directly into an 8 GiB host Metal aperture above RAM, eliminating guest memory bloat. Alignment is matched to Apple Silicon's 16 KiB pages.
- **Message-horizon polling and idle protection:** During active inference streams, lighter dynamically elevates vCPU threads and accelerator workers to `QoS::UserInteractive`. To eliminate round-trip latency without wasting idle CPU, the guest kernel polls for 1 ms following small RPC messages (`qos::Boost`) and automatically backs off when polling does not catch events. Background containers like Frigate NVR run at just 15.4% of a core instead of spinning host CPU.

---

## Features

- **Apple Silicon hardware acceleration:** Native access to Apple Silicon GPU (Vulkan and Metal/ggml), Neural Engine (ANE via ONNX Runtime), PyTorch MPS, and the media engine (video decode and encode over V4L2) in containers via standard Docker CDI (`--device lighter.sh/...`). See the [Hardware & AI Acceleration guide](docs/gpu.md).
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
                                 ├── lighter-docker   Docker socket bridge and port forwarder
                                 └── Accelerators     Metal (ggml), Venus/MoltenVK, CoreML (ANE), MPS, VideoToolbox (V4L2)
```

The guest environment consists of:
- A custom 6.18 longterm Linux kernel booting uncompressed directly from memory (no bootloader).
- Minimal Alpine-based root filesystem with `dockerd` and a lightweight Rust guest agent.
- Host-accelerated virtio-gpu (Venus), ANE CoreML bridge, in-process Metal ggml RPC, PyTorch MPS server, and V4L2 VideoToolbox decoder and encoder.

See [`docs/architecture.md`](docs/architecture.md) and [`docs/gpu.md`](docs/gpu.md) for detailed internals.

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

# 2. Build accelerator dependencies (optional, for GPU/ANE/Metal support)
make gpu ane metal

# 3. Build lighter CLI and VMM, ad-hoc signed with hypervisor entitlement
make build

# 4. Run milestone verification gates
make gates
```

Milestone gates (`make gates`) boot real test VMs to verify end-to-end functionality: kernel boot, device negotiation, Docker engine readiness, network egress, shared filesystem coherency, and memory reclamation.

---

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
