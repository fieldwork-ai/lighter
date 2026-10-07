# 🔥 lighter

**The fast, open-source container engine for macOS, with native Apple Silicon GPU, Neural Engine, and PyTorch acceleration.**

<a href="https://fieldwork.ai">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/fieldwork-logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/fieldwork-logo-light.svg">
    <img alt="Fieldwork" src="assets/fieldwork-logo-light.svg" height="24">
  </picture>
</a>

*lighter is an open-source project sponsored by [Fieldwork](https://fieldwork.ai), providing dedicated engineering time to build and maintain high-performance virtualization and AI infrastructure for Apple Silicon.*

lighter is a container engine for the Mac, built from scratch in Rust on Apple's `Hypervisor.framework`. It is the fastest way to run Docker on Apple Silicon, and the **only one that gives Linux containers Metal, the Neural Engine and the Mac's hardware video encode and decode**.

A drop-in replacement for Docker Desktop, OrbStack and Colima:
- 🔄 **Drop-in Docker:** your existing `docker`, `docker compose`, `kind` and CI scripts work unchanged.
- ⚡ **Fast:** a cold start to a running container in 771 ms; shared folders that read faster than the Mac's own disk; installs on a shared folder as fast as on the Mac itself.
- 🚀 **Apple Silicon acceleration:** local LLMs on Metal, PyTorch on MPS, ONNX models on the Neural Engine and H.264/HEVC on the media engine, all from inside a container.
- 🏠 **Home lab ready:** Frigate supports lighter upstream, Home Assistant's voice pipeline runs on the Mac's GPU, Zigbee and Z-Wave sticks plug straight into your containers, and Pi-hole serves your whole network.
- 🧩 **x86-64 images too:** `linux/amd64` containers run under Apple's Rosetta.
- 🪶 **Light:** 687 MiB with a container running (Docker Desktop: 3.4 GB), and memory goes back to macOS within a minute of work finishing.
- 🆓 **Free and open source:** MIT / Apache 2.0, no subscriptions, no seat licences, no telemetry, no GUI.

*Requires Apple Silicon and macOS 15+ (Sequoia, Tahoe).*

---

## At a glance

| | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|
| **Licence** | **MIT / Apache 2.0** | Proprietary | Proprietary | MIT | Apache 2.0 | Apache 2.0 |
| **Commercial use** | **Free** | $8–$10 / user / mo | $9–$24 / user / mo (≥250) | Free | Free | Free |
| **Telemetry** | **None** | Yes | Yes | None | Opt-in (Podman Desktop) | None |
| **GUI** | **None (headless)** | Menu bar app | Electron app | None | Optional (Podman Desktop) | None |
| **Docker CLI and Compose** | **Yes** | Yes | Yes | Yes | Yes (compatible API) | Through a third-party bridge |
| **x86-64 images** | **Yes (Rosetta)** | Yes | Yes | Yes | Yes | Yes |
| **Kubernetes** | kind, kubectl, Helm | Built-in | Built-in | k3s | kind, `podman kube` | No |
| **GPU in containers (Vulkan)** | **Yes** | No | No | No | Yes | No |
| **llama.cpp on Metal** | **Yes** | No | No | No | No | No |
| **Neural Engine (ONNX)** | **Yes** | No | No | No | No | No |
| **PyTorch on the Mac's GPU (MPS)** | **Yes** | No | No | No | No | No |
| **Hardware video decode and encode** | **Yes** | No | No | No | No | No |

---

## Install

```bash
brew tap fieldwork-ai/tap
brew install lighter
```

Or with the one-line installer:

```bash
curl -fsSL https://raw.githubusercontent.com/fieldwork-ai/lighter/main/scripts/install.sh | sh
```

Then:

```bash
lighter start                               # boots the machine and sets up your Docker context
docker run --rm alpine echo "hello from lighter"
```

```bash
lighter status      # machine state, CPUs, memory and disk
lighter doctor      # check entitlements, Rosetta, accelerators and disk
lighter config      # CPUs, memory, disk, devices
lighter install     # start at login (launchd)
lighter upgrade     # move to the latest release
```

Direct installs can download updates in the background (`lighter update auto-download on`); Homebrew installs are managed by `brew`. See [updates](docs/updates.md).

---

## Benchmarks

Seven ways to run a container on a MacBook Pro (M5 Pro, 18 cores, 48 GB, macOS 26), every runtime at 8 vCPUs and 16 GiB, medians of three, recorded with `benchmarks/compare.sh` on lighter 0.10.0. The method, the raw results and every caveat are in [the record](benchmarks/results/machines/m5/compare-2026-09-27/README.md).

**On a folder shared from the Mac**

| | Mac itself | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|---|
| npm install | 6.5 s | **6.5 s** | 8.8 s | 17.6 s | 17.1 s | 87.8 s | 19.3 s |
| pnpm install | 4.5 s | **4.1 s** | 5.2 s | 27.6 s | 24.7 s | 46.6 s | 28.2 s |
| yarn install | 5.3 s | **5.3 s** | 7.8 s | 24.8 s | 21.2 s | 73.6 s | 24.1 s |
| ripgrep over the tree | 949 ms | **81 ms** | 1023 ms | 3843 ms | 2839 ms | 27376 ms | 3700 ms |
| find over the tree | 330 ms | **85 ms** | 453 ms | 1506 ms | 1378 ms | 38322 ms | 1277 ms |
| copy the tree | 13.7 s | **3.3 s** | 9.3 s | 27.8 s | 39.5 s | failed | 37.4 s |
| rm -rf the tree | 3.8 s | **2.7 s** | 3.2 s | 8.4 s | 8.0 s | failed | 7.6 s |
| write 1 GiB, fsynced | 139 ms | 268 ms | **261 ms** | 704 ms | 791 ms | 482 ms | 851 ms |

**On the runtime's own disk**

| | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|
| npm install | **4.4 s** | 6.7 s | 7.6 s | 7.1 s | 8.3 s | 6.7 s |
| pnpm install | 1.1 s | 1.8 s | 2.7 s | **1.0 s** | 1.7 s | 1.4 s |
| yarn install | **4.0 s** | 5.0 s | 9.7 s | 5.2 s | 5.6 s | 6.4 s |
| ripgrep over the tree | **82 ms** | 101 ms | 113 ms | 113 ms | 126 ms | 95 ms |
| find over the tree | **89 ms** | 118 ms | 116 ms | 162 ms | 123 ms | 106 ms |
| copy the tree | **0.93 s** | 0.99 s | 2.28 s | 1.07 s | 1.20 s | 1.28 s |
| rm -rf the tree | 0.37 s | 0.47 s | 0.39 s | 0.45 s | 0.62 s | **0.28 s** |
| write 1 GiB, fsynced | 384 ms | 407 ms | 342 ms | 501 ms | 285 ms | **242 ms** |

**Engine**

| | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|
| container start | **148 ms** | 297 ms | 154 ms | 163 ms | 188 ms | 970 ms |
| boot to first container | **771 ms** | 1539 ms | 2231 ms | 8860 ms | 8251 ms | 1368 ms |
| memory, one idle container | **687 MiB** | 901 MiB | 3,381 MiB | 1,292 MiB | 2,161 MiB | 705 MiB |
| memory, peak during an install | 9,946 MiB | 5,752 MiB | 6,515 MiB | 8,174 MiB | 14,863 MiB | **3,972 MiB** |
| memory, a minute after it | **1,159 MiB** | 1,705 MiB | 6,452 MiB | 8,174 MiB | 14,865 MiB | –† |
| idle CPU | 5 ms/s | 2 ms/s | 39 ms/s | 6 ms/s | 14 ms/s | **1 ms/s** |
| TCP, Mac to container (Gbit/s) | **106.0** | 101.3 | 25.8 | 4.6 | 5.1 | 31.5 |
| TCP, container to Mac (Gbit/s) | **99.0** | 56.6 | 15.2 | 3.9 | 1.9 | 92.1 |
| TCP, published port (Gbit/s) | **99.6** | 55.8 | 14.8 | 4.0 | 1.8 | 61.0 |
| UDP (Gbit/s) | **5.1** | 3.0 | failed | 3.4 | failed | failed |
| HTTP GET on a published port, median | **63 µs** | 75 µs | 117 µs | 215 µs | 203 µs | 110 µs |
| DNS lookup | **38 µs** | 245 µs | 446 µs | 446 µs | 546 µs | 234 µs |
| a host change seen in a container | **2 ms** | 10 ms | 11 ms | **2 ms** | **2 ms** | 997 ms |
| sha256 of 1 GiB (CPU) | **3.0 s** | 5.5 s | 3.1 s | 3.1 s | 3.2 s | 3.1 s |

† Apple container runs a VM per container, and none once it exits.

lighter keeps a container's file cache for 30 s after it stops, so the next command reads what the last one wrote from memory; that is its higher peak, and it has given the memory back a minute later.

**Media and an LLM**

| | Mac itself | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|---|
| transcode to H.264, 10 s of 1080p30 | 1.36 sᵐ | **1.24 sᵐ** | 4.32 s | 4.05 s | 3.99 s | 4.19 s | 3.92 s |
| transcode to HEVC, the same | 1.42 sᵐ | **1.30 sᵐ** | 5.19 s | 4.96 s | 4.75 s | 4.94 s | 4.50 s |
| zstd -9, 256 MiB, 8 threads | 0.50 s | 0.56 s | 0.59 s | **0.55 s** | 0.60 s | 0.62 s | 0.56 s |
| LLM on the CPU (Qwen2.5 0.5B, 8 threads) | 2.01 s | **2.49 s** | 2.58 s | 2.70 s | 2.67 s | 3.23 s | 2.61 s |

ᵐ on the Mac's media engine, with output identical to the Mac's own; every other runtime encodes in software.

**An LLM on the GPU** (llama.cpp, Qwen2.5 0.5B Q4_K_M, tokens a second)

| | prompt | generation |
|---|---|---|
| Mac itself, Metal | 18,481 | 347 |
| **lighter, Metal** | **16,561** | **294** |
| lighter, Vulkan | 6,760 | 279 |
| Podman, Vulkan | 609 | 260 |

OrbStack, Docker Desktop, Colima and Apple container give a container no GPU.

---

## Apple Silicon acceleration

Five devices, each a standard Docker CDI device (`--device lighter.sh/<name>=all`), on by default. The [guide](docs/gpu.md) has examples, limits and how each works.

| Device | What a container gets | Measured |
|---|---|---|
| `lighter.sh/metal` | llama.cpp, whisper.cpp and other ggml models on Metal | 294 t/s on the M5, 90% of the Mac's prompt rate |
| `lighter.sh/ane` | ONNX models on the Neural Engine, GPU or CPU, fastest chosen | YOLOv9-s in 13 ms against 110 ms on the CPU; ResNet-50 in 2.2 ms |
| `lighter.sh/mps` | PyTorch's `mps` device, training included | a training step in 7 ms on the M5 |
| `lighter.sh/video` | V4L2 H.264, HEVC and VP9 decode, H.264 and HEVC encode, on the media engine | 4K H.264 decode at 16% of a core against 69% in software |
| `lighter.sh/gpu` | Vulkan through virtio-gpu Venus | 6,760 t/s prompt in llama.cpp |

```bash
docker run --rm --device lighter.sh/metal=all -v ./models:/models llama-cpp-rpc \
  llama-bench -m /models/qwen2.5-0.5b-instruct-q4_k_m.gguf --rpc "$LIGHTER_METAL" -ngl 99
```

## Home lab: Frigate, Home Assistant, Pi-hole

- **Frigate supports lighter upstream** ([frigate#24453](https://github.com/blakeblackshear/frigate/pull/24453), in Frigate's next release): its `onnx` detector runs on the Neural Engine, and `preset-apple-silicon-h264` / `-h265` decode cameras on the media engine. On an 8 GB M1, one detector keeps up with 14 cameras (YOLOv9-s), against 3 for Frigate's ZMQ detector on the Mac. For today's Frigate, [`examples/frigate-ane`](examples/frigate-ane) adds the detector to Frigate's own image.
- **Home Assistant** runs as it does anywhere else, with its media, databases and config on shared folders. Its voice pipeline's speech-to-text runs on the Mac's GPU: [`examples/whisper-metal`](examples/whisper-metal) is a Wyoming whisper service that transcribes an 11 s clip in 1.3 s, against 5.8 s on the CPU.

- **Host networking and discovery:** a `network_mode: host` container is reachable from the Mac like a published port. For device discovery (mDNS and SSDP), LAN mode gives the machine its own address on your network: `sudo lighter lan enable`, then `lighter config --lan on` and `lighter restart`. See the [0.12.0 release notes](docs/release-notes-0.12.0.md).

- **Zigbee and Z-Wave sticks:** `lighter usb attach <vendor:product>` hands a stick plugged into the Mac to the guest, under the `/dev/serial/by-id` name Linux gives it. See the [USB guide](docs/usb.md).

- **Pi-hole and AdGuard Home** publish DNS on port 53 (`-p 53:53/udp -p 53:53/tcp`); point your router at the Mac and every device on the network is covered. Published ports pass on each caller's own address, so Pi-hole shows your devices one by one (set its listening mode to all origins: `FTLCONF_dns_listeningMode: all`).

## x86-64 images

`docker run --platform linux/amd64 …` runs under Apple's Rosetta, the translator that runs Intel apps on the Mac, with x86's memory ordering switched on per thread in the guest's kernel. `lighter rosetta --install` fetches Rosetta if the Mac doesn't have it, and `lighter doctor` checks it. See [x86-64](docs/x86-64.md).

| x86-64 image, own disk (M5, lighter 0.5.0) | lighter | OrbStack | Colima | Docker Desktop |
|---|---|---|---|---|
| `npm ci` | **9.23 s** | 13.14 s | 12.73 s | 14.28 s |
| `sha256sum` of 1 GiB | **4.11 s** | 7.92 s | 4.26 s | 4.39 s |

---

## Features

- **Docker and Compose:** a registered Docker context; `docker`, `docker compose` and third-party tooling work unchanged.
- **Kubernetes with kind:** single- and multi-node clusters with `kind`, `kubectl` and `helm`. See the [Kubernetes guide](docs/kubernetes.md).
- **Ports and IPv6:** published ports (`-p 8080:80`) bind on the Mac; IPv6 wherever the Mac has it.
- **Shared folders that behave:** changes on either side seen in milliseconds.
- **Cooperative resources (experimental):** `lighter config --resources cooperative` uses every core and grows memory as containers need it, up to twice the Mac's RAM, giving it back when they stop. See the [0.10.0 release notes](docs/release-notes-0.10.0.md).
- **Headless:** a background daemon or `launchd` service; no menu bar, no Electron.

How it is built, and why it is fast: [architecture](docs/architecture.md).

---

## Building from source

Needs Rust 1.85+, Docker (for the guest kernel and rootfs) and the macOS 15+ SDK.

```bash
make guest            # guest kernel and rootfs
make gpu ane metal    # accelerator libraries (optional)
make build            # the CLI and VMM, ad-hoc signed with the hypervisor entitlement
make gates            # the milestone gates: real VMs, end to end
```

---

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
