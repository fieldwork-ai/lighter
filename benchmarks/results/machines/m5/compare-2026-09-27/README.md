# Seven ways to run a container on the M5 (2026-09-27): lighter 0.10.0 as released

An M5 Pro MacBook Pro (18 cores, 48 GB) on macOS 26.6.2, recorded overnight on 26–27 September with the Mac's own lighter stopped throughout. Every runtime ran at 8 vCPUs and 16 GiB, three repetitions a case, medians below. All used the same fixture (1,232 packages), the same cases (`benchmarks/run.sh`), and the same image, loaded from one archive (`benchmarks/image-dir.sh`) and checked by its layers on each engine. Every run was one `benchmarks/compare.sh --machine m5 --gpu --quality`: one runtime at a time with the rest stopped, the Mac settled before each case, and each guest's size read from inside it. The CSVs and provenance (`.tree`) files are beside this note.

- **lighter** 0.10.0 as released (source 73da44c, kernel 9396cca3, rootfs c3290910), fixed resources, its default. The GPU and quality extras ran on the installed CLI in a home of their own (`LIGHTER_BENCH_EXTRAS_HOME`), on the same kernel and rootfs, so the Mac's daily driver was never started.
- **OrbStack** 2.2.3: `cpu 8`, `memory_mib 16384`.
- **Docker Desktop** 4.89.0: `Cpus` 8, `MemoryMiB` 16384.
- **Colima** 0.10.3: `--cpu 8 --memory 16 --vm-type vz --mount-type virtiofs`.
- **Podman** 6.1.2 from its official installer, on libkrun (krunkit 1.3.2), through its Docker-compatible API.
- **Apple container** 1.4.1, reached through socktainer 1.2.1, so its numbers include socktainer's. Every run was given `--cpus 8 --memory 16384m`.
- **The Mac itself** ran the same cases natively, for reference.
  - Its CPU rows are not comparable: macOS's `shasum` uses the SHA instructions and the image's `sha256sum` does not, and ffmpeg uses all 18 cores.
  - Its DNS row is macOS's resolver, not a container's.
- **Two passes, one night.** OrbStack's rows come from the first pass (00:20–00:38). The first pass was stopped when a fix to the guest agent changed the rootfs (see the worklog for 2026-09-27). The Mac, lighter, Docker Desktop, Colima, Podman, Apple container and the extras were recorded again in the second pass (01:10–04:30).

**On a folder shared from the Mac**

| | Mac itself | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|---|
| npm install | 6.5 s | 6.5 s | 8.8 s | 17.6 s | 17.1 s | 87.8 s | 18.9 s |
| pnpm install | 4.5 s | 4.1 s | 5.2 s | 27.6 s | 24.7 s | 46.6 s | 25.8 s |
| yarn install | 5.3 s | 5.3 s | 7.8 s | 24.8 s | 21.2 s | 73.6 s | 24.1 s |
| ripgrep over the tree | 949 ms | 81 ms | 1023 ms | 3843 ms | 2839 ms | 27376 ms | 3670 ms |
| find over the tree | 330 ms | 85 ms | 453 ms | 1506 ms | 1378 ms | 38322 ms | 1269 ms |
| copy the tree | 13.7 s | 3.3 s | 9.3 s | 27.8 s | 39.5 s | failed | 37.5 s |
| rm -rf the tree | 3.8 s | 2.7 s | 3.2 s | 8.5 s | 8.0 s | failed | 7.7 s |
| write 1 GiB, fsynced | 139 ms | 268 ms | 261 ms | 704 ms | 791 ms | 482 ms | 825 ms |

**On the runtime's own disk**

| | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|
| npm install | 4.4 s | 6.7 s | 7.6 s | 7.1 s | 8.3 s | 6.7 s |
| pnpm install | 1.1 s | 1.8 s | 2.7 s | 1.0 s | 1.7 s | 1.4 s |
| yarn install | 4.0 s | 5.0 s | 9.7 s | 5.2 s | 5.6 s | 6.4 s |
| ripgrep over the tree | 82 ms | 101 ms | 113 ms | 113 ms | 126 ms | 95 ms |
| find over the tree | 89 ms | 118 ms | 116 ms | 162 ms | 123 ms | 106 ms |
| copy the tree | 0.93 s | 0.99 s | 2.28 s | 1.07 s | 1.20 s | 1.28 s |
| rm -rf the tree | 0.37 s | 0.47 s | 0.39 s | 0.45 s | 0.62 s | 0.28 s |
| write 1 GiB, fsynced | 384 ms | 407 ms | 342 ms | 501 ms | 285 ms | 242 ms |

**Engine**

| | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|
| container start | 148 ms | 297 ms | 154 ms | 163 ms | 188 ms | 1081 ms |
| boot to first container | 771 ms | 1539 ms | 2231 ms | 8860 ms | 8251 ms | 1440 ms |
| memory, one idle container | 687 MiB | 901 MiB | 3,381 MiB | 1,292 MiB | 2,161 MiB | 705 MiB |
| memory, nothing running | 606 MiB | 894 MiB | 4,271 MiB | 1,300 MiB | 2,149 MiB | 17 MiB† |
| memory, peak during an install | 9,946 MiB | 5,752 MiB | 6,515 MiB | 8,174 MiB | 14,863 MiB | 3,835 MiB |
| memory, 15 s after it | 9,413 MiB | 2,711 MiB | 6,447 MiB | 8,174 MiB | 14,865 MiB | 28 MiB† |
| memory, a minute after it | 1,159 MiB | 1,705 MiB | 6,452 MiB | 8,174 MiB | 14,865 MiB | 28 MiB† |
| idle CPU | 5 ms/s | 2 ms/s | 39 ms/s | 6 ms/s | 14 ms/s | 1 ms/s |
| idle wakeups a second | 60 | 74 | 4303 | 50 | 56 | 19 |
| TCP, Mac to container | 106.0 Gbit/s | 101.3 Gbit/s | 25.8 Gbit/s | 4.6 Gbit/s | 5.1 Gbit/s | 34.0 Gbit/s |
| TCP, container to Mac | 99.0 Gbit/s | 56.6 Gbit/s | 15.2 Gbit/s | 3.9 Gbit/s | 1.9 Gbit/s | 66.9 Gbit/s |
| TCP, published port | 99.6 Gbit/s | 55.8 Gbit/s | 14.8 Gbit/s | 4.0 Gbit/s | 1.8 Gbit/s | failed |
| UDP | 5.1 Gbit/s | 3.0 Gbit/s | failed | 3.4 Gbit/s | failed | failed |
| connections a second | 17.9k | 18.6k | 18.0k | 18.0k | 16.6k | 21.6k |
| HTTP GET on a published port, median | 63 µs | 75 µs | 117 µs | 215 µs | 203 µs | failed |
| the same, p99 | 123 µs | 113 µs | 165 µs | 261 µs | 290 µs | failed |
| DNS lookup | 38 µs | 245 µs | 446 µs | 446 µs | 546 µs | 222 µs |
| a host change seen in a container | 2 ms | 10 ms | 11 ms | 2 ms | 2 ms | 2 ms |
| sha256 of 1 GiB (CPU) | 3.0 s | 5.5 s | 3.1 s | 3.1 s | 3.2 s | 3.1 s |
| eight sha256 streams at once | 1.4 s | 1.7 s | 1.4 s | 1.4 s | 1.5 s | 1.4 s |

**Media and an LLM**

| | Mac itself | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|---|
| zstd -9, 256 MiB, 1 thread | 3.30 s | 3.58 s | 3.53 s | 3.51 s | 3.58 s | 3.68 s | 3.49 s |
| zstd -9, 256 MiB, 8 threads | 0.50 s | 0.56 s | 0.59 s | 0.55 s | 0.60 s | 0.62 s | 0.56 s |
| transcode to H.264 at 8 Mbit/s, 10 s of 1080p30 | **1.37 s**ᵐ | **1.24 s**ᵐ | 4.32 s | 4.05 s | 3.99 s | 4.19 s | 3.92 s |
| transcode to HEVC at 8 Mbit/s, the same | **1.42 s**ᵐ | **1.30 s**ᵐ | 5.19 s | 4.96 s | 4.75 s | 4.94 s | 4.50 s |
| LLM on the CPU: Qwen2.5 0.5B Q4_K_M, 512 in / 128 out, 8 threads | 2.01 s | 2.49 s | 2.58 s | 2.70 s | 2.67 s | 3.24 s | 2.61 s |

ᵐ on the media engine. Every other runtime has none, and encodes in software (x264 preset medium, x265 preset fast, 8 threads).

- **The transcodes**, scored against the source on the Mac with one scorer (`benchmarks/transcode-check.sh`):

  | | Mbit/s | VMAF | PSNR (Y) | SSIM |
  |---|---|---|---|---|
  | H.264, Mac (VideoToolbox) | 8.40 | 83.45 | 35.52 | 0.9923 |
  | H.264, lighter (V4L2 to VideoToolbox) | 8.40 | 83.45 | 35.52 | 0.9923 |
  | H.264, software (x264, in Podman) | 7.98 | 94.26 | 40.53 | 0.9979 |
  | HEVC, Mac | 9.59 | 90.85 | 38.11 | 0.9959 |
  | HEVC, lighter | 9.59 | 90.85 | 38.11 | 0.9959 |
  | HEVC, software (x265, in Podman) | 7.85 | 95.02 | 41.43 | 0.9982 |

  lighter's output scores exactly as the Mac's, so its time is for the same work.
  - It is 3.2× faster than the best software runtime at H.264 and 3.5× at HEVC, and a little ahead of the Mac.
  - At the same bitrate, the media engine's quality is lower than x264's and x265's, as hardware encoders' is.

**The LLM on a GPU** (llama-bench's own tokens a second, every layer offloaded; `benchmarks/llm-gpu.sh`)

| tokens a second | prompt, 512 | generation, 128 |
|---|---|---|
| Mac itself, Metal (llama.cpp 1af554f, built for the Mac) | 18,481 | 347 |
| lighter, Metal (`lighter.sh/metal`) | 16,561 | 294 |
| lighter, Vulkan (`lighter.sh/gpu`) | 6,760 | 279 |
| Podman, Vulkan (krunkit's virtio-gpu) | 609 | 260 |
| lighter, the same image on the CPU, 8 threads | 695 | 294 |
| Podman, the same image on the CPU, 8 threads | 619 | 240 |

- **Metal through lighter** reads the prompt at 90% of the Mac's rate and generates at 85%: every token crosses ggml's RPC between the container and the Mac.
- **Vulkan:** lighter's reads the prompt at 11× Podman's.
- **No GPU elsewhere:** OrbStack, Docker Desktop, Colima and Apple container offer none to a container.

## Notes

- **Every guest measured itself** at 8 CPUs and 15,939–16,107 MiB (`guest.*` in each `.tree`). No engine had a container running. The Mac's llama.cpp (b11191, commit 4b1a27f) and zstd (1.5.7) are the image's releases, built for the Mac.
- **What lighter trades for its share speed is memory right after work.** The guest keeps its file cache for 30 s after the last container stops, so the next command reads what the last one wrote from memory.
  - The gain: ripgrep over a freshly installed tree reads at 81 ms, where it took seconds from the Mac on 0.9.3.
  - The cost: peak memory is higher, and 15 s after an install lighter holds 9.4 GiB against OrbStack's 2.7.
  - A minute after, it holds 1.2 GiB against OrbStack's 1.7.
- **First repetitions:** the harness settles the Mac for longer than those 30 s before each case, so lighter's first repetition of a read is a cold one (find-walk 352 ms, then 85 and 84; pnpm 5.9 s, then 4.1 and 3.9). The medians are the warm reads.
- **The 1 GiB write** varied by up to 153% between repetitions on every runtime, the Mac included. Read it as an order of magnitude.
- **A failure is the runtime's own.** A timed workload that fails is reported as failed.
  - Podman's shared folder: the tree copy took 274 s twice and then passed the case's 300 s cap, and `rm -rf` of it could not be measured.
  - UDP moved no traffic on Docker Desktop or Podman, and failed on Apple container, whose published-port and HTTP cases also failed through socktainer.
- **Memory at rest is compared with one idle container running**, the first memory row: lighter holds the least of the seven (687 MiB, Apple container 705, OrbStack 901). † Apple container runs a VM per container and none otherwise, so with nothing running it is its services alone (17 MiB), and after the install the install's VM has exited (28 MiB): those readings are of no VM, not of a smaller one. Its own-disk read cases varied by up to 994% between repetitions.
- The 2026-09-26 record beside this one was taken on the 0.10.0 candidate before the share and memory-loop work; this one replaces it for 0.10.0.
