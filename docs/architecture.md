# How lighter is built

The short version: a virtual machine monitor on Hypervisor.framework, a Linux guest built from source in this repository, and a shared filesystem that is fast because it can be told when it is wrong.

## Why not Virtualization.framework

Apple ships two levels. Virtualization.framework gives you a whole virtual machine with a few lines of Swift — and gives you Apple's devices, Apple's virtio-fs, Apple's ideas about memory, and Rosetta. Hypervisor.framework gives you `hv_vm_create`, `hv_vcpu_run`, a way to map memory, and nothing else.

Everything interesting about running containers on a Mac is in the parts the higher level does not expose. The filesystem numbers in the README come from a caching policy driven by FSEvents and a patch to the guest's virtio-fs driver; the memory numbers come from noticing that guest-dirtied pages cannot be reclaimed while the second-stage mapping exists. Neither is reachable through an API that hands you the device.

The cost is Rosetta, which is bound to the higher-level framework by a check we will not work around. [`x86-64.md`](x86-64.md) has that story.

## The crates

Each keeps one secret, in the sense Parnas meant: the thing you would have to change if that decision changed, and the thing nothing else is allowed to know.

| crate | its secret |
|---|---|
| `lighter-hv` | that there is an Apple framework underneath. Bindings, and safe wrappers over `hv_vm_*`, `hv_vcpu_*`, `hv_gic_*`. |
| `lighter-vmm` | what the guest's machine *is*: where memory and devices sit, how a kernel is loaded, what the device tree says, how a core starts and stops, how a device access is serviced. |
| `lighter-fs` | how a host directory becomes a directory in the VM: the FUSE protocol, the macOS syscalls answering it, and the caching policy in between. |
| `lighter-docker` | what Docker's API looks like, and which ports a container has published. |
| `lighter-cli` | presentation, process lifecycle, and everything a person types. |

`lighter-vmm` knows there is something that turns a FUSE request into a reply; it does not know what FUSE is. `lighter-fs` knows nothing about virtqueues. That seam is not decoration — milestone 5 replaced the entire caching policy and added a notification channel without the device model changing at all.

## The machine

Construction order is not stylistic; it is the order the framework and the arm64 boot protocol require, and `machine.rs` comments each step with what breaks if it moves.

1. The VM, because nothing else can be created without it.
2. The interrupt controller — **before the first vCPU**, because it allocates per-core state and refuses once a vCPU has claimed it.
3. The memory map, derived from the geometry the GIC reported rather than written down twice.
4. Guest RAM, then the kernel and its device tree.
5. Devices, on a flat MMIO bus.
6. One thread per core, each creating its own vCPU because `hv_vcpu_create` binds to the calling thread.

There is no firmware. The device tree is the only description the kernel gets of the machine it woke up on, and every address in it comes from the same `GuestLayout` the `hv_vm_map` calls do — because a disagreement is not a crash, it is a device that probes, finds nothing, and is silently absent.

### Devices

virtio-mmio rather than PCI: no host bridge to model, no enumeration, and a device set fixed at boot by the device tree.

- **block** — a sparse file, with discard becoming `F_PUNCHHOLE`, which is what makes deleting an image give space back.
- **net** — frames to `gvproxy`, a userspace network stack running as a sidecar. It is the one component we did not write, consumed over a documented socket protocol so it can be replaced by a native stack without anything else moving.
- **vsock** — the Docker socket and the control channel, independent of the guest's routing table.
- **fs** — one device per shared directory. The requests leave the vCPU thread immediately, because a syscall on a vCPU stops that core and a package install makes hundreds of thousands of them.
- **balloon** — with free page reporting, which is how memory comes back.
- **rng**, **console**.

## The guest

Built from source, not borrowed. `guest/kernel/` holds a configuration and two patches; `guest/rootfs/` an Alpine tree, dockerd, and an init that fits on two screens; `guest/agent/` a small Rust program that bridges vsock to the Docker socket and answers the control channel.

The two kernel patches are the interesting part:

- **A notification queue for virtio-fs.** Linux's driver has a high-priority queue and request queues, both guest-to-host, so FUSE's reverse channel — the one a server uses to say "forget what you cached" — has nowhere to go. Without it, a share can only be cached for a duration chosen in advance and chosen for the worst case. With it, the timeouts are thirty seconds and a host edit still lands in milliseconds.
- **Spinning for a reply instead of sleeping for one.** A synchronous FUSE request costs far more to deliver than the server takes to answer: an interrupt, a work item, and two trips through the scheduler for work that finished microseconds ago. Ten microseconds of spinning took a package install from eighteen seconds to thirteen and a half.

Both are in `guest/kernel/patches/`, each with the reasoning in its header.

## The one invariant worth naming

**How long the guest may cache depends on whether it can be corrected.**

If the notification channel is live, timeouts are seconds and staleness is however long FSEvents takes to notice. If it is not — an unpatched kernel, or one that declined the feature — they fall back to a hundred milliseconds and the share is merely slower. Nothing has to be configured, and there is no combination of guest and host that is fast and wrong.

## Testing

The unit tests are fast and prove much less than the gates. Each gate is a script that boots a real machine and checks a real claim:

| gate | the claim |
|---|---|
| m1 | a kernel we built reaches a shell on the serial console |
| m2 | block, entropy and balloon work; the disk grows and gives space back |
| m3 | `docker run` and `docker compose` work from the macOS CLI |
| m4 | a host directory and a guest agree about it, including under `kill -9` |
| m5 | the filesystem is fast, measured against macOS in the same session |
| m6 | memory comes back, and idling costs nothing |
| m7 | x86-64 containers run |
| m8 | a day's stack survives a night's sleep |

GitHub's macOS runners are themselves virtual machines with no nested virtualization, so CI runs the unit tests and nothing that starts a guest. The gates run on real hardware. That is a limitation, and it is better stated than papered over with mocks.
