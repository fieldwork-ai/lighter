# lighter 0.10.0

Mac-native resources, an experimental mode: containers use the Mac's cores and memory the way native apps do, and memory goes back to macOS when they stop using it. Nothing changes unless you turn it on.

## Mac-native resources (experimental)

```
lighter config --resources native
lighter restart
```

Every Mac container runtime, lighter included, asks for a CPU count and a memory size and walls that slice off from the Mac. In native mode lighter stops asking: every core is a vCPU, and the memory ceiling is the Mac's RAM less 2 GiB. `--cpus` and `--memory` still work, as limits. `lighter config --resources fixed` goes back, and the fixed machine, the default, is exactly 0.9.3's.

**Cores.** The vCPUs run at the default QoS class, the one a native build runs at, so a container build competes with the Mac as a native build would, no harder. On an M5 with all eighteen vCPUs busy, a Mac thread woken every 5 ms was late by 3.6 ms at the 99th percentile, against 6.0 ms behind eighteen native threads doing the same work.

**Memory.** The guest boots on a 2 GiB base and grows by virtio-mem, 128 MiB blocks plugged as it needs them and unplugged when it does not. It keeps a headroom of a quarter of its size (a gigabyte at least) ahead of demand, doubles when containers are short, and gives spare blocks back on its own. The balloon, free-page reporting and the Mac's memory-pressure response all keep working inside whatever is plugged. Linux's page array, 1.56% of the memory the kernel is given, is paid only for what is plugged: a 48 GB ceiling taken whole would cost 750 MB of the Mac's memory at idle.

**Bursts slow, they do not OOM.** A process can take memory faster than the Mac can be asked for more: a tmpfs writer fills a 3 GiB base in 140 ms, and the kernel's answer to memory it cannot reclaim is the OOM killer. So the containers' `memory.high` follows the edge of what the guest can give them, and a burst that reaches it is slowed in reclaim while the guest grows, which is how Meta's Senpai and TMO steer memory. If the Mac stops giving (the ceiling, or macOS short itself), the throttle lifts after three seconds and the kernel does what it would have done without it. Page cache never meets the edge; only memory nothing can reclaim does.

Measured on an M5 (gate m6n, 18 vCPUs, a 16 GiB ceiling):

| | |
|---|---|
| boot | 2 s, 640 MiB of 16384 plugged, footprint 310 MiB |
| a 6000 MiB tmpfs burst from the base | 4 s, nothing OOM-killed (throttled 127 times), range grew to 6912 MiB |
| 300,000 files | 1 s, engine answering promptly |
| after the work | footprint back to 499 MiB within 5 s; range 6912 → 2560 MiB |
| idle CPU | 0.38% |
| Mac thread p99 lateness, all vCPUs busy | 3.6 ms (6.0 behind native threads; 1.3 quiet) |

The real CLI on the M5 reads 18 cores and a 47104 MiB ceiling; `lighter status` shows what is plugged against it.

### Limits

- A process that sizes itself from `MemTotal` at start (a JVM's default heap, some databases) sees what is plugged, the base and the headroom, not the ceiling. Give it a size (`-Xmx`, `shared_buffers`) or use a fixed machine.
- About half of a large growth onlines as kernel memory, so the kernel's own allocations always have room (what the guest starvation of 2026-09-20 lacked), and a kernel-memory block holding a kernel page cannot be unplugged. Its pages are free and go back to the Mac; what stays is its page array, about 0.8% of the peak ever plugged.
- The ceiling is also capped by the guest address space the hypervisor allows on the chip; lighter picks the narrowest one that fits, before the VM exists.

`docs/architecture.md`, "Mac-native resources", has the design.

## Also

- Gate m3's compose stack pulls MinIO from Bitnami's archive (`bitnamilegacy/minio`, the same 2025-04-22 release, pinned): MinIO withdrew its own images from quay.io and Docker Hub.
- The guest kernel carries memory hotplug again, with patches 0024 (memmap on memory) and 0026 (auto-movable onlining); a fixed machine never plugs anything and is unaffected. Linux remains **6.18.52**; the data epoch remains **1**.
