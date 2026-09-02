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

- **block** — a sparse file, with discard becoming `F_PUNCHHOLE`, which is what makes deleting an image give space back. Every request is one `preadv`/`pwritev` straight over the guest's pages; mapping the image was measured and lost (see `Disk`).
- **net** — frames to `gvproxy`, a userspace network stack running as a sidecar. It is the one component we did not write, consumed over a documented socket protocol so it can be replaced by a native stack without anything else moving.
- **vsock** — the Docker socket and the control channel, independent of the guest's routing table.
- **fs** — one device per shared directory. The requests leave the vCPU thread immediately, because a syscall on a vCPU stops that core and a package install makes hundreds of thousands of them.
- **balloon** — with free page reporting, which is how memory comes back.
- **rng**, **console**.

## The guest

Built from source, not borrowed. `guest/kernel/` holds a configuration and four patches; `guest/rootfs/` an Alpine tree, dockerd, and an init that fits on two screens; `guest/agent/` a small Rust program that bridges vsock to the Docker socket and answers the control channel.

The kernel patches are the interesting part:

- **A notification queue for virtio-fs.** Linux's driver has a high-priority queue and request queues, both guest-to-host, so FUSE's reverse channel — the one a server uses to say "forget what you cached" — has nowhere to go. Without it, a share can only be cached for a duration chosen in advance and chosen for the worst case. With it, the timeouts are thirty seconds and a host edit still lands in milliseconds.
- **Spinning for a reply instead of sleeping for one.** A synchronous FUSE request costs far more to deliver than the server takes to answer: an interrupt, a work item, and two trips through the scheduler for work that finished microseconds ago. Spinning took a package install from eighteen seconds to thirteen and a half.

  How long to spin depends on what the other end is doing, and the spin must not take a lock to ask. The queue's lock is the one every other thread needs to *submit*, so a spin that grabs it to check for a reply starves the queue it is waiting on — worth 75 microseconds a create against 59 with sixteen threads at work. It is asked lock-free now, and the window is a hundred microseconds rather than ten: with the device watching the ring there is no longer a trap to rendezvous inside, so the spin has to cover the operation rather than the delivery.

- **Not asking for `security.capability` the server has promised to clear.** `FUSE_HANDLE_KILLPRIV_V2` is a connection-wide undertaking that the server strips setuid, setgid and file capabilities on write. The kernel sends `FUSE_WRITE_KILL_SUIDGID` to say so, and then reads the attribute itself anyway — two round trips per written file, ten percent of all requests on an install, every one answered ENODATA.
- **Not asking questions CREATE already answers.** Creating a file invalidates the parent directory's attributes — which the driver itself just changed, so the next walk through that directory pays a GETATTR to learn what it already knew. With the server's create dialect negotiated, the driver advances the parent's times in place (4,721 GETATTRs on the small npm fixture become 24), and skips the pre-create LOOKUP where the dentry is still unhashed. The skip needs one honesty guarantee in return: `FMODE_CREATED` suppresses the guest-side permission check, so the server creates with `O_EXCL` first and reports whether it really created, refuses a directory with the EISDIR that `open(2)` would give anyway, and answers a trailing symlink with ELOOP — on which the driver falls back to the ordinary lookup path so the VFS can walk it.

All four are in `guest/kernel/patches/`, each with the reasoning in its header.

## The guest decides how much the host remembers

The server keeps a descriptor per directory the guest has looked up, and one per file while there is room, because a walk of an absolute path on every operation is what makes shared directories slow everywhere else. But the guest is under no obligation to forget: nothing pressures a dentry cache in an idle VM, so a tree walked once is remembered forever — and a stock Mac allows a process 10,240 descriptors (`kern.maxfilesperproc`), which one pnpm install walks straight through. Nick's machine allows 184,320, which is how a share that thrashed on every other Mac measured well on his.

So the descriptor is treated as a cache of where the file lives, never as the file's identity. Every inode remembers the parent and name the guest last reached it by, and everything a regular file needs has an `*at` form that takes a directory descriptor and a name — so a file past the budget is operated on through its parent's descriptor at the cost of the same one syscall as a resident one, a directory past the budget is revived through its own parent with one `openat` per cold level, and a regular file the guest looks up into a full share is registered with no descriptor at all rather than opened only for the sweep to close it. The name is a hint and never trusted: what it answers is checked against `(dev, ino)` on every use, and when the Mac has moved the name on the inode is reopened by identity through macOS's `/.vol` namespace, with the file's immutable birth time as the check that a recycled inode number is not impersonating it. A budget proportional to the ceiling bounds how many descriptors stay open; a clock sweep parks the cold ones, directories last, and escalates in three regimes — deferring to recency while the budget holds, pacing itself while a hot working set breathes above it, and parking regardless of recency at a red line short of the ceiling, because past that the next event is `EMFILE` inside the guest on a file that is plainly there. Any change here is measured at `LIGHTER_FS_FD_BUDGET=6144`, the stock budget, before it is believed.

## The one invariant worth naming

**How long the guest may cache depends on whether it can be corrected.**

If the notification channel is live, timeouts are seconds and staleness is however long FSEvents takes to notice. If it is not — an unpatched kernel, or one that declined the feature — they fall back to a hundred milliseconds and the share is merely slower. Nothing has to be configured, and there is no combination of guest and host that is fast and wrong. Host changes also include a previous server's: fseventsd delivers a killed VMM's final writes into the stream of the one that boots next on the same share, seconds late, and a withdrawn entry can land between a guest's `unlink`s and its `rmdir`. The guest then forgets the directory's inode and looks the name up again — so an inode whose names are still promises (a queued create, a queued removal) outlives the guest's memory of it, like a pending inode does, and the lookup finds it rather than a second inode that never heard of those promises.

## The guest is not made to wait for APFS

A package install is six hundred thousand filesystem requests issued one at a time, and through a synchronous server the guest spent the full APFS latency of each one blocked: forty-seven microseconds for a create, forty for an unlink, ten for a write. Measured on the large fixture that serial track *was* the wall clock — 10.5 of 10.7 seconds — while the guest's own compute, the 7.7 seconds an install takes on the guest's local disk, ran underneath it and was never the limit. The guest gains nothing by waiting. It only ever sees the outcome through later filesystem operations, and the server can answer those itself.

So creates, writes, unlinks, renames, links, directories, symlinks, removals and clones are acknowledged as soon as their outcome is known and performed later on the apply queue (`crates/lighter-fs/src/apply.rs`). A pending create is a registry inode with a provisional identity and the attributes it was promised; a pending directory holds nothing but promises, and lookups and listings under it are answered from its overlay; lookups everywhere consult a per-directory overlay of promised and promised-away names before asking the host; sizes and link counts come from overlays while jobs are queued. Three promises hold it up, in order of importance: reads never lie (anything whose answer a queued job could change either consults the overlay — read *before* the host is consulted, never after, or a job applying in between withdraws the promise the host has not caught up with yet — or waits on the inode's own flags, not the whole queue); durability is never claimed early (`fsync`, `syncfs` and `DESTROY` drain fully — after them the host answers for everything); and errors are not swallowed (a failed job parks its errno on the inode for the next operation to report; an `rmdir` checks emptiness before it is acknowledged; near a full disk acknowledgement stops and service goes back to synchronous). What the guest gives up is a visibility window: a file a container wrote reaches the Mac milliseconds after the write returned rather than before, and a create nothing has forced within fifty milliseconds is made by a settler thread. The other direction — the Mac's changes reaching the guest — is untouched, and the server stays authoritative over every attribute, which is what keeps the writeback class of bugs out. `LIGHTER_FS_ASYNC=0` restores synchronous service.

**A job names what it touches, and that is the only order there is.** Every job carries the nodeids of the inodes it changes — the directory, the file, what it displaces — and two jobs sharing one are applied in the order they were queued; jobs sharing nothing are free of each other. That is exactly the order the overlays reason about, so the queue can be more than one thread where APFS rewards it, and measured, it rewards it in one place: clones and copies, whose cost is inside the file, and removals, which are keyed by their directory and so only ever overlap across directories. Everything else — creates, writes, renames, directories — runs one at a time, because four workers applying everything were slower at everything: APFS hands a directory between threads at a cost each time. Three workers is where clone throughput stops improving.

**A create is held, not queued, until something needs the file.** pnpm opens every file it imports and then clones over it, and writes every store file under a temporary name, closes it and renames it into place. Held back, the first is withdrawn by the clone for nothing, and the second becomes one job that creates the final name with the bytes and the chmod in it: three jobs for each of fifty thousand files became one, and the temporary name never exists on the Mac. A write, a rename, a link, a read, a barrier or the settler forces the create; the chmod libuv sends between an open and a clone is kept by the create itself, under the lock the job is taken with.

**A small clone is a copy.** An APFS clone costs sixty to a hundred microseconds whatever is done around it — whether the source is fresh, settled or fsynced, by descriptor or by path — and the volume serves no more of them per second for being asked in parallel. A create with the bytes written into it costs about the same alone and does scale across directories, which is how pnpm's four import threads arrive. Files under a size cap are copied; larger ones are cloned and share their blocks as FICLONE promises. Hardlinks, which pnpm falls back to on every other Docker, cost a hundred and sixty-six microseconds each here — twice a clone — and were measured and rejected.

**The registry must be able to breathe.** A parked directory taxes every name inside it, so directories are residency-privileged in every sweep. At a full share a new file is bound parked rather than opened and closed again a moment later. And the lookup count must match the kernel's exactly, in both directions: the server finding its own entry for a name is not a lookup, and a reply that names no nodeid — a clone's reply is a size — owes none, so the inode it made is born without one. Counting one too many left every unlinked file a FORGET short of release and a tree walk taking sixteen minutes; later it left every imported file in the registry forever, sixty thousand an install, and a server that got slower with each repetition. A test now forgets each shape by the count the kernel would use and asks the registry to be empty.

## Measuring

`docs/measuring.md` — which instrument answers which question, what can be
resolved at all, and the three explanations in here that turned out to be
wrong.

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
