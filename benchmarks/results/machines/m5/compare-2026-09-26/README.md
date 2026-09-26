# Seven ways to run a container on the M5 (2026-09-26): the 0.10.0 release record

An M5 Pro MacBook Pro (18 cores, 48 GB) on macOS 26.6.2, one session, every other runtime stopped before each (the harness's guard on). Every runtime at 8 vCPUs and 16 GiB, three repetitions a case, medians below, the same fixture (1,232 packages), the same cases (`benchmarks/run.sh`), and the same image, loaded from one archive (`benchmarks/image-dir.sh`) and checked by its layers on each engine. The CSVs and provenance (`.tree`) files are beside this note.

- **lighter** 0.10.0 (b19a134, the release plus the benchmark changes), fixed resources, its default.
- **OrbStack** 2.2.3, `cpu 8`, `memory_mib 16384`, 8 CPUs confirmed in the guest.
- **Docker Desktop** 4.89.0, `Cpus` 8, `MemoryMiB` 16384; `docker info` reported 8 CPUs, 15.6 GiB.
- **Colima** 0.10.3, `--cpu 8 --memory 16 --vm-type vz --mount-type virtiofs`.
- **Podman** 6.1.2 from its official installer, on libkrun (krunkit 1.3.2), through its Docker-compatible API.
- **Apple container** 1.4.1 through socktainer 1.2.1 (its numbers include socktainer's). Every run given `--cpus 8 --memory 16384m`; its VM gets one vCPU more than asked, for its init, and the container's cgroup is limited to exactly 8 (`cpu.max` 800000/100000).
- **The Mac itself** runs the same cases natively, for reference; its CPU rows are not comparable (macOS's `shasum` uses the SHA instructions, the image's `sha256sum` does not; ffmpeg uses all 18 cores).

**On a folder shared from the Mac**

| | Mac itself | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|---|
| npm install | 7.0 s | 6.5 s | 9.0 s | 17.8 s | 17.5 s | 88.7 s | 19.3 s |
| pnpm install | 4.7 s | 3.8 s | 5.2 s | 28.0 s | 24.7 s | 47.2 s | 26.1 s |
| yarn install | 5.1 s | 4.9 s | 7.8 s | 24.8 s | 21.2 s | 73.7 s | 24.4 s |
| ripgrep over the tree | 936 ms | 84 ms | 1051 ms | 3823 ms | 5407 ms | 30835 ms | 3696 ms |
| find over the tree | 352 ms | 85 ms | 446 ms | 1485 ms | 1366 ms | 38102 ms | 1257 ms |
| copy the tree | 15.0 s | 3.6 s | 9.7 s | 27.8 s | 42.2 s | 275.6 s | 37.4 s |
| rm -rf the tree | 3.9 s | 2.2 s | 3.2 s | 8.5 s | 8.1 s | – | 7.7 s |
| write 1 GiB, fsync | 143 ms | 274 ms | 268 ms | 681 ms | 663 ms | 703 ms | 737 ms |

**On the runtime's own disk**

| | Mac itself | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|---|
| npm install | n/a | 4.5 s | 6.6 s | 7.7 s | 7.2 s | 8.3 s | 6.5 s |
| pnpm install | n/a | 1.1 s | 1.8 s | 2.7 s | 1.0 s | 1.6 s | 1.5 s |
| yarn install | n/a | 4.0 s | 4.9 s | 9.9 s | 5.5 s | 5.5 s | 6.5 s |
| ripgrep over the tree | n/a | 81 ms | 111 ms | 105 ms | 104 ms | 124 ms | 94 ms |
| find over the tree | n/a | 90 ms | 117 ms | 119 ms | 175 ms | 126 ms | 122 ms |
| copy the tree | n/a | 0.8 s | 1.0 s | 2.3 s | 1.1 s | 1.2 s | 1.5 s |
| rm -rf the tree | n/a | 0.4 s | 0.5 s | 0.4 s | 0.5 s | 0.6 s | 0.4 s |
| write 1 GiB, fsync | n/a | 370 ms | 386 ms | 359 ms | 694 ms | 153 ms | 291 ms |

**Engine**

| | Mac itself | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|---|
| sha256 of 1 GiB, one core | 0.4 s | 3.0 s | 5.5 s | 3.1 s | 3.1 s | 3.2 s | 3.1 s |
| sha256, 8 streams at once | 0.1 s | 1.4 s | 1.7 s | 1.4 s | 1.4 s | 1.5 s | 1.4 s |
| container start | – | 165 ms | 298 ms | 159 ms | 163 ms | 192 ms | 1021 ms |
| boot to first container | – | 760 ms | 1463 ms | 2272 ms | 8039 ms | 8321 ms | 1361 ms |
| memory, nothing running | – | 623 MiB | 918 MiB | 4,315 MiB | 1,299 MiB | 2,086 MiB | 17 MiB |
| memory, one idle container | – | 700 MiB | 915 MiB | 3,408 MiB | 1,293 MiB | 2,095 MiB | 705 MiB |
| memory, peak during an install | – | 3,999 MiB | 5,682 MiB | 6,766 MiB | 10,754 MiB | 21,673 MiB | 3,901 MiB |
| memory, a minute after it | – | 1,052 MiB | 1,793 MiB | 6,706 MiB | 10,765 MiB | 21,677 MiB | 32 MiB |
| idle CPU (ms/s) | – | 4 | 3 | 42 | 6 | 15 | 1 |
| idle wakeups a second | – | 55 | 33 | 4246 | 59 | 62 | 19 |
| TCP, Mac to container | 127.4 Gbit/s | 109.5 Gbit/s | 101.6 Gbit/s | 26.2 Gbit/s | 4.9 Gbit/s | 4.7 Gbit/s | 34.0 Gbit/s |
| TCP, container to Mac | 135.4 Gbit/s | 101.5 Gbit/s | 55.7 Gbit/s | 15.4 Gbit/s | 4.1 Gbit/s | 1.8 Gbit/s | 92.0 Gbit/s |
| TCP, into a published port | – | 92.5 Gbit/s | 57.7 Gbit/s | 14.9 Gbit/s | 4.1 Gbit/s | 1.8 Gbit/s | 58.9 Gbit/s |
| TCP, out of a published port | – | 103.6 Gbit/s | 96.3 Gbit/s | 34.8 Gbit/s | 4.8 Gbit/s | 32.9 Gbit/s | 31.0 Gbit/s |
| UDP | 22.9 Gbit/s | 5.2 Gbit/s | 3.2 Gbit/s | failed | 3.5 Gbit/s | failed | – |
| new connections a second | 26913 | 17970 | 21273 | 17680 | 17694 | 16551 | 13997 |
| HTTP GET, median | 39 µs | 62 µs | 75 µs | 116 µs | 230 µs | 204 µs | 108 µs |
| DNS lookup | 7942 µs | 38 µs | 248 µs | 476 µs | 458 µs | 553 µs | 231 µs |

**Media and an LLM (CPU only; image `lighter-bench:2`, llama.cpp b11191)**

| | Mac itself | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|---|
| x264, 20 s of 1080p30 (medium) | 1.2 s | 2.2 s | 2.5 s | 2.3 s | 2.2 s | 2.3 s | 2.1 s |
| x265, 5 s of 1080p30 (fast)¹ | 1.0 s | 3.5 s | 3.5 s | 3.7 s | 3.5 s | 1.4 s | 1.3 s |
| LLM: Qwen2.5 0.5B Q4_K_M, 512 in / 128 out, 8 threads | 2.2 s | 2.5 s | 2.5 s | 2.7 s | 2.6 s | 3.2 s | 2.6 s |

¹ x265 under Docker's engine (lighter, OrbStack, Docker Desktop, Colima) runs without a thread pool: Docker's default seccomp profile allows the NUMA memory-policy calls (`get_mempolicy`, `set_mempolicy`, `mbind`) only with `CAP_SYS_NICE`, libnuma's probe fails, and x265 then allocates no pool rather than falling back to the CPU count ("No thread pool allocated, --wpp disabled"). In lighter with `--cap-add SYS_NICE` or `--security-opt seccomp=unconfined` the same encode takes 1.29 s. Podman's and Apple's defaults do not block the calls.

**Media v2, partial (paused when the M5 was needed)**

The second media pass adds a fair CPU case (zstd, the same code on the Mac and in a guest), hardware transcodes (VideoToolbox on the Mac, V4L2 through `lighter.sh/video` in lighter) and a real clip (Big Buck Bunny, 10 s of 1080p30 H.264) in place of ffmpeg's pattern generator, which was the limit. It ran on the Mac and on lighter 0.10.0 with the seccomp change before stopping; OrbStack ran zstd-1 only. The Mac's row (`m5-native-media2.csv`, commit b0336f9) is on the clip; lighter's and OrbStack's (commit d08b4fe) are on the pattern, so their transcodes are not comparable with the Mac's, and only the zstd rows are.

| | Mac itself | lighter | OrbStack |
|---|---|---|---|
| zstd -9, 256 MiB, 1 thread | 3.50 s | 3.51 s | 3.55 s |
| zstd -9, 256 MiB, 8 threads | 0.55 s | 0.55 s | – |

A guest's CPU is the Mac's CPU: on a fair case the three are within 1.5%. The sha256 cases above measure two different sha256 implementations, the Mac's and the image's, not two CPUs.

On lighter, the pattern: x265 1.30 s with the seccomp change (3.5 s in the table above without it), and HEVC on the media engine 0.63 s. Still to run: every container runtime on the clip, then `benchmarks/llm-gpu.sh`.

## Notes

- **A failure is the runtime's own.** The harness only unblocks untimed setup; a timed workload that fails is reported as failed. Podman's shared folder leaves every hard-linked file of a pnpm tree behind on `rm -rf`, so its rm -rf failed, and its third copy-tree hit the per-case time limit (the first two took 276 s). UDP moved no traffic on Docker Desktop, Podman or Apple container.
- **Podman's memory** (21.7 GB at the peak of an install and a minute after it, more than its 16 GiB guest) is the footprint of its processes as macOS accounts it, read the same way as every other runtime's; what in libkrun holds the excess is not yet known.
- **Apple container's memory** a minute after the install is its services alone: the install's container has exited, taking its VM with it. A running container's VM keeps its peak (it attaches no memory balloon).
- **ripgrep on the share** reads a freshly written tree; each runtime's first pass differs in how much of it it kept cached.
