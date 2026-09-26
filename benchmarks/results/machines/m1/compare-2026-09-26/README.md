# lighter, Podman and Apple container on the M1 (2026-09-26)

An 8 GB M1 Mac mini on macOS 26.6.2. Every runtime at 8 vCPUs and 4 GiB: Podman's machine made with `podman machine init --cpus 8 --memory 4096`, and every Apple container run given `--cpus 8 --memory 4096m`, since each of its containers is a VM of its own (4 CPUs and 1 GB unless told). Three repetitions a case, medians below, the same fixture (1,232 packages) and the same cases (`benchmarks/run.sh`). The CSVs and provenance (`.tree`) files are beside this note.

- **lighter** 0.10.0 at 2b6ef0c, fixed resources (its default).
- **Podman** 6.1.2 from its official macOS installer, on its default provider on Apple silicon, libkrun (krunkit 1.3.2), driven through its Docker-compatible API.
- **Apple container** 1.4.1, driven through socktainer 1.2.1, the community bridge that gives it a Docker API. Its numbers include socktainer's own cost.

Podman and Apple container ran a prebuilt benchmark image loaded from one archive (`benchmarks/image-dir.sh`, image `sha256:815656aa…`); lighter's share and own-disk rows are from the 0.10.0 requalification record the same day, which built the same Dockerfile itself.

| On a folder shared from the Mac | lighter | Podman | Apple container |
|---|---|---|---|
| npm install | 11.7 s | 66.4 s | 25.7 s |
| pnpm install | 7.3 s | 51.6 s | 47.7 s |
| yarn install | 11.3 s | failed | 32.5 s |
| ripgrep over the tree | 4213 ms | 132 ms | 12209 ms |
| find over the tree | 101 ms | 2656 ms | 3617 ms |
| copy the tree | 5.6 s | failed | 58.2 s |
| rm -rf the tree | 2.7 s | failed | 11.4 s |

| On the runtime's own disk | lighter | Podman | Apple container |
|---|---|---|---|
| npm install | 7.6 s | 12.5 s | 11.8 s |
| pnpm install | 1.7 s | 2.4 s | 1.9 s |
| yarn install | 7.9 s | 8.5 s | 11.3 s |
| ripgrep over the tree | 159 ms | 218 ms | 174 ms |
| find over the tree | 122 ms | 160 ms | 157 ms |
| copy the tree | 4.9 s | 5.9 s | 3.7 s |
| rm -rf the tree | 0.6 s | 1.1 s | 0.5 s |

| Engine | lighter | Podman | Apple container |
|---|---|---|---|
| container start | 202 ms | 249 ms | 1611 ms |
| boot to first container | 922 ms | 9544 ms | 1748 ms |
| idle memory, nothing running | 322 MiB | 1,499 MiB | 17 MiB |
| idle memory, one idle container | 400 MiB | 1,504 MiB | 465 MiB |
| idle CPU | 7 ms/s | 7 ms/s | 1 ms/s |
| TCP, Mac to container | 59.1 Gbit/s | 1.9 Gbit/s | 23.5 Gbit/s |
| TCP, published port | 49.6 Gbit/s | 1.6 Gbit/s | 30.9 Gbit/s |
| UDP | 5.1 Gbit/s | failed | failed |
| HTTP GET on a published port, median | 135 µs | 257 µs | 163 µs |
| DNS lookup | 117 µs | 775 µs | 468 µs |
| sha256 of 1 GiB (CPU) | 6.3 s | 6.4 s | 6.3 s |

## Notes

- **Podman's failures are its own.** On its shared folder, pnpm keeps its store at the folder's root and hard-links every installed file to it (as it does on every runtime here), and `rm -rf` of such a tree leaves every hard-linked file behind ("Directory not empty"), on every pass: so the timed `rm -rf` fails. yarn and the tree copy fail with `EMFILE` / "No file descriptors available" from its shared-folder server. The harness only unblocks untimed setup (a tree it cannot delete is moved aside and deleted from the Mac, `benchmarks/cases/clear.sh`); a timed workload that fails is reported as failed. UDP moved no traffic on either Podman or Apple container.
- **ripgrep on the share:** lighter's first passes read a freshly written tree cold from the Mac (7.5 s and 4.2 s, then 0.14), and Podman's are warm from the first (0.12–0.18), because its shared folder keeps what the guest wrote cached in the guest. Warm against warm they are level (135 ms against 132).
- **Idle memory:** with nothing running, Apple's runtime has no VM at all (17 MiB of services). With one idle container it holds that container's whole VM (465 MiB), and its VMs attach no memory balloon (`apple/containerization`, `VZVirtualMachineInstance.toVZ`), so a running container keeps its peak until it stops. lighter holds 322 MiB at rest and 400 with a container, and returns memory while containers run.
- **One session on one machine**, a Mac with 8 GB, which runs short of memory under the largest cases; the M5 comparison is still to do.
