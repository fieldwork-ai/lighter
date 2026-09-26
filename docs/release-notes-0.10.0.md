# lighter 0.10.0

Mac-native resources, an experimental mode: containers use the Mac's cores and memory the way native apps do, and memory goes back to macOS when they stop using it. Nothing changes unless you turn it on.

## Mac-native resources (experimental)

```
lighter config --resources native
lighter restart
```

Every Mac container runtime, lighter included, asks for a CPU count and a memory size and walls that slice off from the Mac. In native mode lighter stops asking: every core is a vCPU, and the memory ceiling is the Mac's RAM less 2 GiB. `--cpus` and `--memory` still work, as limits. `lighter config --resources fixed` goes back, and the fixed machine, the default, is exactly 0.9.3's.

**Cores.** The vCPUs run at the default QoS class, the one a native build runs at, so a container build competes with the Mac as a native build would, no harder. On an M5 with all eighteen vCPUs busy, a Mac thread woken every 5 ms was late by 3.6 ms at the 99th percentile, against 6.0 ms behind eighteen native threads doing the same work.

**Memory.** The guest boots on a 2 GiB base and grows by virtio-mem, 128 MiB blocks plugged as it needs them and unplugged when it does not. It keeps a headroom of a quarter of its size (a gigabyte at least) ahead of demand, grows by that much again whenever containers are short, and gives spare blocks back on its own. It grows for page cache only into memory the Mac is not using, and while the Mac is under pressure every growth is paced to a quarter-gigabyte every two seconds, so the guest never makes the Mac swap to feed it. The balloon, free-page reporting and the Mac's memory-pressure response all keep working inside whatever is plugged. Linux's page array, 1.56% of the memory the kernel is given, is paid only for what is plugged: a 48 GB ceiling taken whole would cost 750 MB of the Mac's memory at idle.

**Bursts slow, they do not OOM.** A process can take memory faster than the Mac can be asked for more: a tmpfs writer fills a 3 GiB base in 140 ms, and the kernel's answer to memory it cannot reclaim is the OOM killer. So the containers' `memory.high` follows the edge of what the guest can give them, and a burst that reaches it is slowed in reclaim while the guest grows, which is how Meta's Senpai and TMO steer memory. If the Mac stops giving (the ceiling, or macOS short itself), the throttle lifts after three seconds and the kernel does what it would have done without it. Page cache never meets the edge; only memory nothing can reclaim does.

Gate m6n, on each Mac at the ceiling a person would get there (capped at 16 GiB); the M5 was also doing a working day's jobs, which is where its lateness comes from, behind the guest and native threads alike:

| | M5 Pro, 18 vCPUs, 16 GiB | M1, 8 vCPUs, 6 GiB |
|---|---|---|
| boot, plugged / footprint | 640 of 16384 MiB / 333 MiB | 0 of 6144 MiB / 306 MiB |
| a tmpfs burst from the base | 6000 MiB in 4 s, nothing OOM-killed (throttled 343 times) | 3072 MiB in 1 s, nothing OOM-killed (throttled 278 times) |
| the range, peak → after | 11264 → 4736 MiB | 4096 → 1152 MiB |
| 300,000 files | 1 s | 1 s |
| footprint after the work | 554 MiB within 5 s | 450 MiB within 5 s |
| idle CPU | 0.37% | 0.75% |
| Mac thread p99 lateness, all vCPUs busy | 6.86 ms (6.75 behind native threads) | 2.52 ms (2.52 behind native threads) |

The benchmark record on the M1, native (8 vCPUs, 6 GiB ceiling) against fixed (8 vCPUs, 4 GiB), one session: memory held a minute after a large install is 784 MiB against 1482, and 750 against 1550 after fifteen seconds. Installs, container starts, the network and boot are level, and so is everything on the guest's own disk but an npm install (8.6–9.2 s against 7.6). One case is slower: a copy through a share straight after the installs (copy-tree 6.7–8.7 s against 5.4–5.8). On its own it is level (4.5 s against 4.4). What differs is the Mac: after the installs an 8 GB Mac is under pressure, a native guest gives its cache up to it, and a fixed one keeps it, holding twice the memory to do so.

Three things found on the way, in the policy that sizes the guest (see the design notes):

- Page cache filling the guest used to count as the guest being short, and grew it. On the 8 GB M1 that doubled a guest mid-copy, put the Mac into pressure, and the pressure then made the guest evict that very cache: the next pass read the whole tree again, 16.9 s against 4.4. Cache now grows the guest only into memory the Mac is not using.
- A guest short of memory used to double. It now grows by a quarter at a time, and the throttle holds a burst while it does.
- While the Mac is under pressure, any growth is paced to a quarter-gigabyte every two seconds. Over the same install sequence the Mac's paging fell from 3.8 times what a fixed guest cost it to 1.4.

The real CLI on the M5 reads 18 cores and a 47104 MiB ceiling; `lighter status` shows what is plugged against it.

### Limits

- A process that sizes itself from `MemTotal` at start (a JVM's default heap, some databases) sees what is plugged, the base and the headroom, not the ceiling. Give it a size (`-Xmx`, `shared_buffers`) or use a fixed machine.
- About half of a large growth onlines as kernel memory, so the kernel's own allocations always have room (what the guest starvation of 2026-09-20 lacked), and a kernel-memory block holding a kernel page cannot be unplugged. Its pages are free and go back to the Mac; what stays is its page array, about 0.8% of the peak ever plugged.
- The ceiling is also capped by the guest address space the hypervisor allows on the chip; lighter picks the narrowest one that fits, before the VM exists.

`docs/architecture.md`, "Mac-native resources", has the design.

## A fixed machine is 0.9.3's

The default is unchanged, and measured so: gate m6 passes every check on the M1 (the balloon plateaus at 2048 MiB of an 8 GiB guest and eases to 608, as on 0.9.3), and the M1's record is level with 0.9.3's on the share, the network, memory and boot (npm 11.8–12.3 s against 11.2–12.9, find-walk 108 ms against 107, boot 728/896 ms against 731/906, idle 331 MiB against 330). Two rows read low in the record and were run again, alternating 0.9.3 and 0.10 three times, each on its own kernel and rootfs: reverse egress read 51.3–52.1k on 0.10 against 51.3–52.2k on 0.9.3, and a copy on the guest's own disk 3.32 s on average against 3.72. The native policy work that followed touches nothing a fixed machine runs, and the gates and records were run again on the final build: every gate passes on the M1, m6 in full. The memory rows of that record read higher (1482 MiB a minute after the install against 1174), so the memory case alone was run on the two builds alternately, three times each, on the same guest: 604–688 MiB before the change and 609–640 after.

On the way it caught one bug of its own: the guest took the virtio-mem driver, which every kernel now has, for a range, so a fixed 8 GiB guest offered its spare memory to the balloon and throttled its containers. It now asks whether a device is bound.

## x265 gets its threads

Containers may now set their own NUMA memory policy (`get_mempolicy`, `set_mempolicy`, `mbind`), as they can under Podman. Docker's default seccomp profile allows these only with `CAP_SYS_NICE`, and x265, finding libnuma's probe refused, runs without a thread pool: an HEVC encode of five seconds of 1080p took 3.5 s on 8 vCPUs, and takes 1.3 s now. The profile is otherwise Docker's own, and a container's `--security-opt seccomp=` still replaces it. `docs/architecture.md` has the reasoning.

## Also

- Gate m3's compose stack pulls MinIO from Bitnami's archive (`bitnamilegacy/minio`, the same 2025-04-22 release, pinned): MinIO withdrew its own images from quay.io and Docker Hub.
- The guest kernel carries memory hotplug again, with patches 0024 (memmap on memory) and 0026 (auto-movable onlining); a fixed machine never plugs anything and is unaffected. Linux remains **6.18.52**; the data epoch remains **1**.
