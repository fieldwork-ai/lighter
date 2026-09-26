# Six Mac container runtimes on the M1 (2026-09-26)

An 8 GB M1 Mac mini on macOS 26.6.2, one session. Every runtime at 8 vCPUs and 4 GiB, three repetitions a case, medians below, the same fixture (1,232 packages), the same cases (`benchmarks/run.sh`) and the same benchmark image, loaded from one archive (`benchmarks/image-dir.sh`) and checked by its layers on each engine. The CSVs and provenance (`.tree`) files are beside this note.

- **lighter** 0.10.0 at 2b6ef0c, fixed resources (its default). Its share and own-disk rows are from the 0.10.0 requalification record the same day, which built the same Dockerfile itself.
- **OrbStack** 2.2.3 (`orb config set cpu 8`, `memory_mib 4096`; 8 CPUs confirmed in the guest).
- **Docker Desktop** 4.89.0 (settings `Cpus` 8, `MemoryMiB` 4096; `docker info` reported 8 CPUs, 3.8 GiB).
- **Colima** 0.10.3 (`--cpu 8 --memory 4 --vm-type vz --mount-type virtiofs`).
- **Podman** 6.1.2 from its official macOS installer, on libkrun (krunkit 1.3.2), its default on Apple silicon, through its Docker-compatible API.
- **Apple container** 1.4.1 through socktainer 1.2.1, the bridge that gives it a Docker API; its numbers include socktainer's own cost. Every run given `--cpus 8 --memory 4096m`, since each of its containers is a VM of its own (4 CPUs and 1 GB unless told).

**On a folder shared from the Mac**

| | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|
| npm install | 11.7 s | 11.8 s | 28.1 s | 25.3 s | 66.4 s | 25.7 s |
| pnpm install | 7.3 s | 6.2 s | 47.3 s | failed | 51.6 s | 47.7 s |
| yarn install | 11.3 s | 11.1 s | 37.0 s | 29.3 s | failed | 32.5 s |
| ripgrep over the tree | 4213 ms | 1089 ms | 17705 ms | 18583 ms | 132 ms | 12209 ms |
| find over the tree | 101 ms | 555 ms | 4163 ms | 3882 ms | 2656 ms | 3617 ms |
| copy the tree | 5.6 s | 12.6 s | 51.5 s | 62.4 s | failed | 58.2 s |
| rm -rf the tree | 2.7 s | 4.1 s | 12.9 s | 12.2 s | failed | 11.4 s |

**On the runtime's own disk**

| | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|
| npm install | 7.6 s | 10.6 s | 13.3 s | 12.0 s | 12.5 s | 11.8 s |
| pnpm install | 1.7 s | 2.4 s | 2.2 s | 1.7 s | 2.4 s | 1.9 s |
| yarn install | 7.9 s | 7.9 s | 14.1 s | 11.6 s | 8.5 s | 11.3 s |
| ripgrep over the tree | 159 ms | 152 ms | 243 ms | 211 ms | 218 ms | 174 ms |
| find over the tree | 122 ms | 134 ms | 156 ms | 232 ms | 160 ms | 157 ms |
| copy the tree | 4.9 s | 5.8 s | 3.8 s | 3.1 s | 5.9 s | 3.7 s |
| rm -rf the tree | 0.6 s | 0.7 s | 0.8 s | 0.9 s | 1.1 s | 0.5 s |

**Engine**

| | lighter | OrbStack | Docker Desktop | Colima | Podman | Apple container |
|---|---|---|---|---|---|---|
| container start | 202 ms | 296 ms | 238 ms | 220 ms | 249 ms | 1611 ms |
| boot to first container | 922 ms | 1550 ms | 4113 ms | 12473 ms | 9544 ms | 1748 ms |
| memory, nothing running | 322 MiB | 689 MiB | 4,413 MiB | 1,146 MiB | 1,499 MiB | 17 MiB |
| memory, one idle container | 400 MiB | 687 MiB | 4,453 MiB | 1,148 MiB | 1,504 MiB | 465 MiB |
| memory, peak during an install | 3,470 MiB | 5,070 MiB | 4,474 MiB | 4,333 MiB | failed | 2,882 MiB |
| memory, a minute after it | 1,482 MiB | 1,298 MiB | 4,443 MiB | 4,295 MiB | failed | 24 MiB |
| idle CPU | 7 ms/s | 17 ms/s | 27 ms/s | 10 ms/s | 7 ms/s | 1 ms/s |
| idle wakeups a second | 60 | 96 | 1762 | 54 | 74 | 9 |
| TCP, Mac to container | 59.1 Gbit/s | 58.5 Gbit/s | 12.3 Gbit/s | 4.1 Gbit/s | 1.9 Gbit/s | 23.5 Gbit/s |
| TCP, container to Mac | 51.5 Gbit/s | 28.3 Gbit/s | 9.6 Gbit/s | 3.1 Gbit/s | 1.5 Gbit/s | 25.7 Gbit/s |
| TCP, published port | 49.6 Gbit/s | 28.8 Gbit/s | 9.5 Gbit/s | 2.9 Gbit/s | 1.6 Gbit/s | 30.9 Gbit/s |
| UDP | 5.1 Gbit/s | 3.0 Gbit/s | failed | 2.4 Gbit/s | failed | failed |
| HTTP GET on a published port, median | 135 µs | 126 µs | 194 µs | 422 µs | 257 µs | 163 µs |
| DNS lookup | 117 µs | 394 µs | 854 µs | 708 µs | 775 µs | 468 µs |
| sha256 of 1 GiB (CPU) | 6.3 s | 9.2 s | 6.4 s | 6.5 s | 6.4 s | 6.3 s |

## Notes

- **A failure is the runtime's own.** The harness only unblocks untimed setup (a tree a setup cannot delete is moved aside and deleted from the Mac, `benchmarks/cases/clear.sh`); a timed workload that fails is reported as failed.
  - Podman's shared folder: pnpm keeps its store at the folder's root and hard-links every installed file to it (as it does on every runtime here), and `rm -rf` of such a tree leaves every hard-linked file behind, on every pass. yarn and the tree copy fail with `EMFILE` / "No file descriptors available" from its shared-folder server. The memory case, which installs, did not complete.
  - Colima's pnpm install failed on all three repetitions in the suite's order, where it installs over the previous case's tree; on a fresh folder it completes (57 s).
  - UDP moved no traffic on Docker Desktop, Podman or Apple container.
- **ripgrep on the share:** lighter's first passes read a freshly written tree cold from the Mac (7.5 s and 4.2 s, then 0.14 s), where Podman and OrbStack keep what the guest wrote cached in the guest. Warm against warm, lighter is at 135 ms, Podman 132, OrbStack 1,078.
- **OrbStack's sha256** (9.2 s against 6.3–6.5 everywhere else) was reproduced with a plain `sha256sum` of 1 GiB in alpine (9.4 s), with 8 CPUs confirmed in the guest.
- **Memory:** with nothing running, Apple's runtime has no VM at all (17 MiB of services); with one idle container it holds that container's whole VM (465 MiB), and its VMs attach no memory balloon (`apple/containerization`, `VZVirtualMachineInstance.toVZ`), so a running container keeps its peak until it stops. Its 24 MiB a minute after the install is the install's container having exited.
- **One session on one machine**, a Mac with 8 GB, which runs short of memory under the largest cases; the M5 comparison is still to do.
