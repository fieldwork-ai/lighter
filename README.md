# lighter

Docker on a Mac, on a virtual machine written for the job.

Not a wrapper around Apple's Virtualization.framework. lighter is a virtual machine monitor built directly on Hypervisor.framework — its own vCPU loop, its own interrupt controller setup, its own device models, its own guest kernel — because the interesting problems in running containers on a Mac are all in the parts a higher-level API does not let you touch.

**MIT licensed. No commercial tier, no paid edition, no "free during beta". That is a promise about the licence, and it is why the licence is the short one.**

Apple Silicon, macOS 15 or later.

## Install

```bash
brew install fieldwork-ai/tap/lighter
lighter start
docker run --rm alpine echo hello
```

`lighter start` brings up the machine and registers a Docker context, so `docker` finds it with nothing exported and nothing to remember. You need the Docker CLI (`brew install docker`); lighter is the daemon, not the client.

```bash
lighter status      # is it running, and what is it costing
lighter doctor      # check this Mac, and say what to fix
lighter config      # cores, memory, disk
lighter install     # start it when you log in
lighter stop
```

## What it is like

Numbers from `benchmarks/`, measured on one machine on one afternoon against the same 1,232-package fixture. Each figure is the median of three runs, as a fraction of the same command run directly on macOS: higher is better, and above 100% means the container beat the Mac's own disk. OrbStack was measured here, by this suite, not quoted.

| | lighter | OrbStack |
|---|---|---|
| `rg` over `node_modules` | **844%** | 94% |
| metadata walk of the same tree | **355%** | 80% |
| `cp -a node_modules` | **207%** | 157% |
| `npm ci` | 52% | **72%** |
| `yarn install` | 44% | **73%** |
| `pnpm install` | 28% | **83%** |
| `rm -rf node_modules` | 75% | **127%** |
| a file changed on the Mac, seen in a container | **5 ms** | 12 ms |

Boot to a served Docker socket: about two seconds. Idle: 0.2% of one core, and around 700 MB — a guest that used eight gigabytes for a build gives them back within five seconds of the build ending.

Read the table honestly, because it says two things. **Reading a shared tree, lighter is between four and nine times faster than the best commercial option**, and faster than the Mac itself: the guest's page cache answers without crossing the VM boundary at all, and Linux's VFS is quicker than the one underneath it. That is only allowed because the cache can be *corrected* — a patch to the guest's virtio-fs driver adds a notification queue, macOS FSEvents says what changed, and an invalidation goes out naming the exact file. Without a channel like that, a shared filesystem has to guess a timeout and live with being wrong for it.

**Writing one, lighter is behind, and by a lot on `pnpm`.** The cost is per-operation and not per-byte: a package install is several hundred thousand small requests, each one a round trip that macOS answers in microseconds and that costs microseconds more to deliver. `benchmarks/README.md` has the measurements, including what was tried and found worthless so nobody repeats it. The same fixture installed on the guest's own disk takes 7.7s against the Mac's 6.6s, which is where the ceiling is if the boundary were free.

Run them yourself:

```bash
benchmarks/run.sh --target native   --reps 5
benchmarks/run.sh --target lighter  --reps 5
benchmarks/run.sh --target orbstack --reps 5
python3 benchmarks/report.py
```

The harness refuses to run while another hypervisor is alive, because a second VM moves the ratio between the two figures and that ratio is the only thing it reports.

## What it does

- **Docker and Compose**, through the real CLI, with published ports appearing on `localhost` as containers start and disappearing as they stop.
- **Bind mounts of your files** at the same paths they have on the Mac, coherent in both directions.
- **x86-64 containers** — `docker run --platform linux/amd64` — under emulation. Not Rosetta; [`docs/x86-64.md`](docs/x86-64.md) explains why that door is closed.
- **Memory that comes back.** The guest hands free pages to macOS as it frees them, and gives up more when the Mac is under pressure.
- **A disk that shrinks.** Deleting an image punches a hole in the host file rather than leaving it grown.

## What it does not do

Kubernetes. A GUI. Intel Macs — not now and not later. Windows or Linux hosts. Rootless containers. If you need those, this is the wrong tool and Colima or Docker Desktop is the right one.

## How it is built

```text
lighter (CLI)  ──spawns──▶  lighter run  ──────────▶  gvproxy (network sidecar)
                                 │
                                 ├── lighter-hv    Hypervisor.framework, and nothing else
                                 ├── lighter-vmm   the machine: memory map, boot, devices, vCPUs
                                 ├── lighter-fs    the shared filesystem, host side
                                 └── lighter-docker  watching what containers publish
```

One crate, one secret, in the sense Parnas meant: `lighter-hv` is the only thing that knows there is an Apple framework underneath, `lighter-fs` is the only thing that knows what a FUSE request looks like, and neither knows about the other. [`docs/architecture.md`](docs/architecture.md) is the longer version.

The guest is ours too: a kernel built from source with a configuration in this repository, two patches to its virtio-fs driver, an init that fits on two screens, and a small Rust agent. `make guest` builds all of it — and `make dogfood` builds it *using lighter*, which is both a good test and the point.

## Building it

```bash
make guest     # kernel, initramfs, root filesystem  (needs a container runtime)
make build     # the VMM and the CLI, signed with the hypervisor entitlement
make gates     # every milestone gate, in order
```

The gates are the tests that matter. Each one is a script that boots a real machine and checks a real claim — that a kernel we built reaches a shell, that a disk gives space back, that `docker compose` goes green, that a host and a guest agree about a directory, that memory returns, that an amd64 container runs, that a day's stack survives a night's sleep. `make test` is the unit tests, which are fast and prove much less.

CI runs the unit tests only: GitHub's macOS runners are themselves virtual machines with no nested virtualization, so nothing that starts a guest can run there. The gates run on real hardware, which is a limitation worth stating plainly rather than working around with mocks.

## Licence

MIT. See [LICENSE](LICENSE).
