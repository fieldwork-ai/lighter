# How lighter is built

lighter is three programs: a virtual machine monitor on the Mac, a Linux guest built from source in this repository, and a small agent inside the guest. The monitor runs the machine and every device; the guest runs Docker; the agent joins the two. What makes it fast is mostly one idea applied everywhere: do the work on whichever side can answer soonest, and let the other side be told when it is wrong.

This document is the map. Each section says what a part does and why it is built that way, with the one or two measurements that decided it. The dated investigations in `docs/` and the rows of [`worklog.md`](worklog.md) hold the rest.

## Why Hypervisor.framework

Apple ships two levels. Virtualization.framework gives you a whole virtual machine in a few lines of Swift, with Apple's devices, Apple's virtio-fs and Apple's ideas about memory. Hypervisor.framework gives you `hv_vm_create`, `hv_vcpu_run`, a way to map memory, an interrupt controller, and nothing else.

Everything that matters for containers on a Mac is in the parts the higher level does not expose: a shared filesystem whose cache is driven by FSEvents and a patched guest driver, and memory that goes back to macOS the moment the guest lets it go. lighter ran on Virtualization.framework for a day to check (2026-09-05): it never returned memory the guest had freed, and a synchronous filesystem request through its helper process cost about 33 µs against 3 to 5 µs here. Rosetta, the reason for trying, works without it ([`x86-64.md`](x86-64.md)).

## The crates

Each keeps one secret: the decision you would change it for, which nothing else is allowed to know.

| crate | its secret |
|---|---|
| `lighter-hv` | that there is an Apple framework underneath: bindings and safe wrappers over `hv_vm_*`, `hv_vcpu_*`, `hv_gic_*` |
| `lighter-vmm` | what the machine is: where memory and devices sit, how a kernel boots, how a device access is served |
| `lighter-fs` | how a Mac directory becomes a directory in the guest: FUSE, the macOS calls that answer it, and the cache policy between them |
| `lighter-docker` | what Docker's API looks like, and which ports a container has published |
| `lighter-cli` | everything a person types, installation, updates, and the machine's lifecycle |

`lighter-vmm` knows something turns a FUSE request into a reply; it does not know what FUSE is. `lighter-fs` knows nothing of virtqueues. That seam let the whole share cache policy be replaced, and a notification channel added, without the device model changing.

## The machine

The construction order is the one the framework and the arm64 boot protocol require, and `machine.rs` comments each step with what breaks if it moves: the VM, then the interrupt controller (before the first vCPU, or it refuses), then the memory map derived from what the controller reported, then RAM, the kernel and its device tree, then the devices, then one thread per vCPU (a vCPU belongs to the thread that made it). There is no firmware: the device tree is the kernel's only description of the machine, and every address in it comes from the same layout the memory mappings use, because a disagreement is not a crash but a device that is silently absent.

Devices are virtio over MMIO: no bus to model, no enumeration, a set fixed at boot.

| device | what it is |
|---|---|
| block | a sparse file per disk, discard becoming `F_PUNCHHOLE`; see *Storage* |
| fs | one per shared directory, served by `lighter-fs`; see *Shared folders* |
| vsock | the Docker socket, the control channel, and every container connection; see *The network is streams* |
| net | a responder for the few frames that still reach a card: ARP, one DHCP lease, ICMP |
| balloon | free page reporting and the balloon; see *Memory* |
| mem | virtio-mem, for cooperative resources only |
| gpu | virtio-gpu with Venus: Vulkan rendered by virglrenderer over MoltenVK (`lighter.sh/gpu`) |
| media | virtio-media: V4L2 decode and encode on VideoToolbox (`lighter.sh/video`) |
| rng, console | as usual |

The Neural Engine, Metal and PyTorch devices are not virtio: they are servers on the Mac that containers reach through the streams, and Docker's CDI names their ports. [`gpu.md`](gpu.md) covers all five accelerators.

Two transport details are easy to get wrong. The interrupt line is level-triggered and follows the status word, re-derived under one lock by whichever side changed it, so raise and acknowledge can interleave freely. And the packed ring's event index runs on free-running counters, not ring positions: a guest that fills the whole ring while the host completes all of it in one pass leaves the positions where they started, which a position test reads as nothing new. Disk and share queues are drained by host threads that watch the rings, so under load no vCPU services a request or waits behind one.

## The guest

The guest is built here, not borrowed: a Linux 6.18 LTS kernel with a measured configuration and 44 patches (`guest/kernel/`), an Alpine root filesystem with dockerd and a short init (`guest/rootfs/`), and the agent (`guest/agent/`), a Rust program that bridges the streams, answers the control channel and runs the memory policy.

The configuration is measured rather than inherited. The arm64 defconfig's full preemption, kernel pointer authentication, stack zeroing, IRQ time accounting and audit cost a tenth of every syscall and every tmpfs create against OrbStack's kernel; without them the guest is a fifth faster on those and a quarter faster copying a tree on its own disk. The kernel ticks at 250 Hz: a 1000 Hz build made a container's life 60 ms against 112, but made share installs slower (pnpm 4.2 to 4.9 s against 3.9 to 4.2), and installs are what people do most.

The patches, by what they are for (each carries its reasoning in its header):

| area | what the patches do |
|---|---|
| shared folders | a notification queue so the Mac can tell the guest to forget (below); spinning briefly for a reply instead of sleeping; not asking what a create already answered; keeping size and times the guest knows after a write or rename; keeping the pages the guest wrote and answering for folders it made (0043 to 0045, *Shared folders*) |
| disk | polling the used ring after a submit, one queue per vCPU, completing a checksum-free btrfs read where it finished, and not flushing a copy while it is still being written |
| idle | polling before WFI, only while it pays (*Idle*) |
| memory | the balloon in compound, movable units; reporting at a host-page order; readahead that does not enter direct reclaim in a full cgroup; virtio-mem onlining for cooperative resources |
| network | the sockmap join's missing pieces and vsock's fallbacks under fragmentation (*The network is streams*) |
| devices | virtio-media fixes, a V4L2 control fix sent upstream, and TSO for Rosetta's threads (0023) |

The engine's seccomp profile is Docker's own with Podman's rule for NUMA memory policy (`get_mempolicy`, `set_mempolicy`, `mbind`), because x265, refused libnuma's probe, runs without a thread pool: an HEVC encode took 3.5 s on 8 vCPUs, 1.3 s with the calls allowed. `scripts/seccomp-profile.py` generates it from Docker's default and CI checks it.

## Shared folders

A package install on a shared folder is hundreds of thousands of filesystem requests, one at a time. Every other runtime pays the Mac's filesystem latency on each. lighter mostly does not, for four reasons.

**The guest caches as long as it can be corrected.** Linux's virtio-fs has no channel for a server to say "forget what you cached", so a share can only be cached for a duration chosen in advance for the worst case. A kernel patch adds a notification queue, and the server drives it from FSEvents: a change on the Mac invalidates the guest's cached name, attributes or pages within milliseconds, and when FSEvents loses detail the guest is told to reset the whole share (a dentry epoch, a version barrier for attributes and data). With the channel live, names and attributes are cached for twenty seconds as a backstop against delivery stalls; without it (an unpatched kernel) they fall back to a hundred milliseconds. Nothing is configured, and no combination of guest and host is fast and wrong. The [event-loss investigation](filesystem-event-loss-2026-09-07.md) has the failure cases.

**The guest is not made to wait for APFS.** Creates, writes, unlinks, renames, links, directories and clones are acknowledged as soon as their outcome is known and applied afterwards by the apply queue (`crates/lighter-fs/src/apply.rs`). The server answers later requests from overlays of what it promised. Three rules hold it up: reads never lie (an answer a queued job could change consults the overlay or waits for that inode), durability is never claimed early (`fsync`, `syncfs` and shutdown drain the queue), and errors are not swallowed (a failed job's error is reported by the next operation; near a full disk, service goes back to synchronous). The cost is a window of milliseconds before a container's file appears on the Mac. Jobs are ordered only by the inodes they touch, so independent ones run side by side where APFS rewards it: creates, writes and removals across directories, and clones. A create is held until something needs the file, so pnpm's write-to-a-temporary-name-then-rename becomes one create of the final name, and a small clone is a copy, because an APFS clone costs 60 to 100 µs however it is done. `LIGHTER_FS_ASYNC=0` restores synchronous service.

**The guest keeps what it wrote (0.10.0).** A tree an install has just written used to be read back from the Mac on the next command. Three patches keep it: the partial last page of a written file is kept, and the guest's own writes do not invalidate its cache; a folder the guest made is complete, so a name it has no entry for does not exist and is answered locally (ripgrep's `.gitignore` in every folder, a package manager's check before each create); and such a folder lists itself from a snapshot taken when the listing starts. Any change from the Mac, or a lost notification, ends all three for what it touched. ripgrep straight after an install went from 5.8 s to 0.15 s on the M1. [`complete-directories-2026-09-26.md`](complete-directories-2026-09-26.md) has the design.

**Descriptors are a cache, never an identity.** The server keeps a descriptor per directory and per file while there is room, because walking an absolute path on every operation is what makes shares slow elsewhere. A stock Mac allows a process 10,240, which one pnpm install walks through, so every inode also remembers the parent and name the guest reached it by, and everything has an `*at` form through the nearest ancestor that holds a descriptor. The name is a hint, checked against the inode's identity before use, with macOS's `/.vol` namespace as the fallback. A clock sweep parks cold descriptors, directories last. Changes here are measured at the stock budget (`LIGHTER_FS_FD_BUDGET=6144`) before they are believed.

**Ownership is recorded, not applied (0.9.3).** The server runs as the Mac user and cannot give a Mac file to anyone else. A container's `chown` is recorded in the extended attribute Docker Desktop uses, `com.docker.grpcfuse.ownership`, and reported in the guest from then on, so images that drop root after chowning their volumes (Frigate, every database) work. Reading a record costs about nine `lstat`s, so it is read only in a directory marked `sh.lighter.ownership`, and a share with no records reads none.

**A file with no recorded owner belongs to whoever asks (0.10.3).** The Mac user's files have no owner a container would recognize. Until 0.10.3 they showed as root's, so a container running as 1000 got only the "other" bits of its own bind mount, and a 770 folder refused it. Docker Desktop and OrbStack show such a file as the caller's, and so does lighter now. The server flags the attribute (`FUSE_ATTR_LIGHTER_CALLER_UID`, `_GID`), and guest patch 0046 answers `stat`, the permission check, `chmod`, time sets and a `chown` to oneself as if the caller owned it. That is per caller, not in the cached attributes, so two containers with different users on one share each see their own. The inode itself still says root, which is what the rest of the kernel sees, as before. A recorded owner is a real one, and clearing it (a `chown` back to root) gives the file back to whoever asks.

## Storage

Each disk is a sparse file on the Mac, sized at what the Mac has free when the machine is made (64 GiB at least), and served with one `preadv` or `pwritev` straight over the guest's pages. Discard punches holes, so deleting an image gives space back. A guest flush is `fsync(2)` of the image, the data at the drive as every Mac runtime gives it, not `F_FULLFSYNC`, which costs 4 ms a call and which a container start used to wait on about eighty times.

The guest formats the data disk as btrfs, mounted `nodatacow`, for reflinks: yarn and `cp` copy trees file by file, and a clone moves no data where ext4 moved 1.4 GB per install. `nodatacow` writes in place, which keeps a database file from fragmenting. btrfs reserves metadata per dirty file at a worst case of a quarter megabyte, so a 75,000-file tree wants 19 GB of room while it is written, which is why the disk is never small. Two upstream reclaim paths took that reservation for a shortage and flushed a copy file by file while it was still being written; one is off and the other plugged, and a 1 GB copy went from 2.9 s of system time to 1.1 s wall.

If the Mac's disk fills, a write that fails with `ENOSPC` is held and retried every three seconds, rather than failing into the guest's filesystem. Writes keep their order; reads of ranges no held write touches still pass, so Docker's page faults do not stall; other devices keep working. `lighter status` shows what is waiting through a status socket that works when Docker cannot answer. `scripts/test-disk-full.py` checks it on a capped disposable image.

## The network is streams

A container's TCP connection never crosses to the Mac as packets. In the guest, netfilter redirects every connection leaving through the network device to the agent, which opens one vsock stream to the Mac per connection, the destination in a 19-byte header; on the Mac each becomes an ordinary socket, so the Mac's routing, VPN and proxy settings apply to it, and there is no TCP stack in lighter to get wrong. Published ports are the same the other way: the Mac binds what Docker bound in the guest (every interface, or loopback with `lighter config --publish localhost`), and each accepted connection is a stream the agent connects to Docker's port. UDP flows share one framed stream per direction, diverted in the guest by TPROXY so each datagram keeps its destination. DNS is a framed stream of its own to the Mac's resolver; in the guest the resolver listens on `:15353` and nat rules send `192.168.127.2:53` to it, which leaves port 53 free for a container to publish (Pi-hole, 0.10.2).

A published port passes on who is calling. The stream's header carries the client's address after the destination's, and the agent binds its socket to that address transparently before connecting to Docker's port, so the container sees the device on the network rather than lighter. The container's replies are addressed to that client: the agent's socket carries a mark, conntrack copies it to the connection, and replies arriving from the container take a mark of their own that a policy rule routes back to the agent's socket, instead of out as outbound traffic (a route on the socket's own mark would send its own packets to loopback). Connections from the Mac's loopback keep arriving from the guest. `scripts/test-inbound-rules.py` holds these rules to the agent's constants.

The agent does not copy the bytes. Once both ends are connected it joins them in the kernel with a BPF sockmap program, and on the Mac one kqueue thread serves every stream. Guest patches supply what sockmap lacked: the end of a stream riding the redirect, flow control reaching the sender, pages moving by reference, a backlog that backs off rather than spinning, and a reader that does not deliver a retransmitted overlap twice (patch 0030, caught by a release gate). Every unix socket the monitor owns is widened past macOS's 8 KiB default, which alone had capped every stream at 13 Gbit/s. On the M5 the TCP paths read 99 to 106 Gbit/s; a kept-alive GET through a published port 63 µs; a DNS lookup 38 µs.

IPv6 rides the same streams: the guest has a unique-local address, containers get addresses in a sibling prefix, and on the Mac the stream connects over IPv6 exactly when the Mac can reach the destination. A v6 publish that Docker refuses is retried at the container's v4 address, because a server bound to `0.0.0.0` would otherwise look broken on `localhost`, which macOS resolves to `::1` first. The card sees only ARP, DHCP and ICMP, answered in the monitor (`net.rs`), which replaced gvproxy; anything else reaching it is a flow that escaped the redirect, and the network gate fails on the count.

## Memory

A fixed machine (the default) gives memory back to macOS four ways, and never makes the Mac page for its cache.

- **Free page reporting**: the guest reports unused pages continuously, in 128 KiB runs at rest, and the monitor replaces their backing so macOS reclaims them. Each 16 KiB host page is its own owned memory object, so a partial release frees real pages and reuse is charged normally; RAM is prepared on first touch, so what the guest never uses costs nothing. The [accounting](memory-accounting-2026-09-06.md) and [demand-memory](demand-memory-2026-09-08.md) investigations have the measurements.
- **The balloon**, when the Mac is short. One ramp follows macOS's pressure level (a small step at Normal while the Mac compresses, a quarter of the guest at Warn, a half at Critical) and eases down slowly, never on a single Normal reading between Warns: handing 4 GiB back in one step once had the Mac page the guest for thirty seconds. A guest that says it is short holds the ramp where it is. The driver inflates in movable compound units from 2 MiB down to 16 KiB, so a balloon of gigabytes is whole blocks and the rest stays compactable.
- **The idle trim**: thirty seconds after the last container stops (and again at thirty-five), the agent reclaims the containers' and engine's file cache down to a sixty-fourth of RAM, compacts, and hurries reporting. Thirty seconds, not three as before 0.10, so the pause between two commands does not throw away what the first wrote. A trim waits for an empty container hierarchy, because a running container's mapped files are never disposable.
- **The idle pass**: page cache untouched for an hour goes back, by DAMON with per-page access bits (anonymous and mapped pages are never candidates). Its progress is read on a thread of its own: reading it asks DAMON to answer at its next sample, up to a minute away, and until 0.10.0 that stalled the whole memory loop once a minute.

While containers run, a warm loop keeps their cache near its working set: every six seconds a small reclaim through `memory.reclaim`, scaled by the Mac's pressure and stopped by the guest's own stall, which is Meta's Senpai (TMO) with its production constants. dockerd and containerd are protected from the OOM killer; BuildKit's workers are reset to ordinary so a large build cannot evict small containers. A guest short of memory must not lose its streams either: a refused 256 KiB vsock packet is sent shorter, and the agent retries a write refused for memory instead of ending the stream.

### Cooperative resources

`lighter config --resources cooperative` (experimental, 0.10) stops sizing the machine: every core is a vCPU, and memory is plugged by virtio-mem as the guest needs it, up to twice the Mac's RAM. `--cpus` and `--memory` become limits.

- **Cores.** vCPUs run at the default QoS class, where a native build runs, so containers compete with the Mac as native work does: with all eighteen busy on the M5, the Mac's interactive thread was as late behind them as behind eighteen native threads. A lower class protected it more but cost a fifth of the guest's throughput.
- **Memory.** The guest boots on a 2 GiB base with none of the range plugged, and init waits only for that base before Docker restores containers (0.10.0 waited for the whole range and could not start with a container saved; fixed in 0.10.1). The policy (`size_range`, `memory_policy.rs`) keeps a headroom of a quarter of the guest ahead of demand, grows on a need, grows for cache only into memory the Mac is not using, and gives spare blocks back. While the Mac is under pressure every growth is a quarter-gigabyte at most every two seconds. The page array, 1.56% of whatever the kernel is given, is paid only for what is plugged.
- **Bursts slow, they do not OOM.** A process can take memory faster than the Mac can be asked for it, so the containers' `memory.high` follows the edge of what the guest can give them (`throttle.rs`), and a burst that reaches it is slowed in reclaim while the guest grows. It lifts after three seconds without growth, so the worst case is what the kernel would have done anyway.
- **Past the Mac's memory.** A native app can ask for more than the Mac has and be compressed and swapped rather than killed; a guest capped below the Mac's memory had its containers OOM-killed instead. What lies past the Mac's memory is memory in use, never cache, and the ceiling is twice rather than unbounded so a runaway container meets the guest's OOM killer before the Mac's swap fills its disk.

A process that sizes itself from `MemTotal` at start (a JVM's default heap) sees what is plugged, not the ceiling; give it a size or use a fixed machine. Gate m6c checks all of it.

## Idle

Under a hypervisor, WFI parks the host thread, and waking it costs the sender a trap and the receiver a scheduler round trip: 19 µs between two vCPUs, where a thread pool hands off constantly. An idle guest CPU therefore spins briefly first, with `TIF_POLLING_NRFLAG` set so a waker writes a flag instead of sending an interrupt (patch 0011): 2.9 µs, and npm on the M5's own disk went from 8.3 s to 5.1. The spin pays only while it catches wakeups (patch 0039): a poll that times out halves the window, a CPU whose spinning costs more than 50 µs per caught wakeup backs off, and the window is shorter where the vCPUs fill the host's cores. A container's stream to an accelerator widens it while a model is talking, which keeps a Metal token loop at full rate. The host's queue pollers back off the same way. An idle machine costs 5 ms of CPU a second on the M5.

## Testing

Unit tests are fast and prove much less than the gates. Each gate is a script that boots a real machine and checks a real claim, and a release passes all of them on the exact build it ships.

| gate | the claim |
|---|---|
| m1 | a kernel we built reaches a shell |
| m2 | block, entropy and balloon work; the disk grows and gives space back |
| m3 | the network, vsock, `docker` and `docker compose`, streams and published ports work from the Mac |
| m4 | a shared folder and the guest agree about it, including under `kill -9` and changes from either side |
| m5 | shared folders and the guest's disk are fast, against macOS in the same session |
| m6 | memory comes back, and idling costs nothing |
| m6c | cooperative resources grow, throttle, give back, restart with a saved container and go past the Mac's memory |
| m7 | x86-64 containers run under Rosetta |
| m8 | a day's stack survives a night's sleep |
| m9 to m14 | the GPU, the Neural Engine, PyTorch, Metal, video decode and video encode |

GitHub's macOS runners are virtual machines without nested virtualization, so CI runs the unit tests and nothing that boots a guest; the gates run on real Macs. Benchmarks are `benchmarks/compare.sh`, one runtime at a time on a settled Mac; [`measuring.md`](measuring.md) says which instrument answers which question.
