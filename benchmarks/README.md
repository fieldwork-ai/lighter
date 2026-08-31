# Benchmarks

```bash
benchmarks/run.sh --target native   --reps 5
benchmarks/run.sh --target lighter  --reps 5
benchmarks/run.sh --target orbstack --reps 5
python3 benchmarks/report.py
```

Results go to `results/<target>.csv`; `report.py` turns them into `RESULTS.md`. Nothing in the report is hand-written, so a number nobody can reproduce cannot appear in it.

## What is being compared

Every target runs the same case scripts against the same fixture — a pinned `node_modules` tree — on the same machine. What differs is only how the directory reaches the process: on `native` it is the Mac's own disk, and everywhere else it is a bind mount through whatever that runtime uses for file sharing.

`native` is the reference. It is a hard reference: a shared filesystem at 100% of it is costing nothing at all next to a local disk, which is not a thing shared filesystems normally are.

## Three rules the harness enforces

**The timing loop runs inside the target.** It used to run outside, around a whole `docker run`, and that measured container startup: a metadata walk costing the filesystem 1,566 requests reported 550ms, of which about 450ms was Docker creating and destroying a container. The native target pays no such cost, so the comparison was not between two filesystems at all. Everything before the first measurement — image pull, container start, a cold page cache — now happens once and is not timed.

**Caches are warmed, and the warming is not timed.** An `npm install` that downloads is measuring the network. Each target gets its own package cache on its own storage, warmed by an untimed run first.

**The median is reported.** Not the mean, which one scheduling hiccup drags around, and not the best, which is a claim about the machine being idle.

## The cases

| case | what it is | what it stresses |
|---|---|---|
| `npm-install` | `npm ci` of a pinned lockfile | creates, writes and renames of small files |
| `ripgrep` | reading every file in `node_modules` | opens and reads |
| `find-walk` | `find -type f` over `node_modules` | lookups and directory reads, no file opened |
| `copy-tree` | `cp -a node_modules node_modules_copy` | read and create together |
| `watch-latency` | a change on the host, seen in the guest | how quickly a cache can be corrected |

## Where lighter stands, and why

The read cases are faster than macOS itself. That is not a trick: the guest's page cache answers without any round trip, and Linux's VFS is quicker than the one underneath it. It is only possible because the cache can be *corrected* — see `crates/lighter-fs/src/notify.rs` and the guest kernel patch — so the timeouts can be thirty seconds instead of a hundred milliseconds while a host edit still lands in single-digit milliseconds.

The write cases are not, and the plan's 85%-of-native target for `npm install` is not met. Two reasons, and neither is a tuning problem:

- **Round trips.** A package install makes about 65,000 filesystem requests. Each one is a trap out of the guest, some work, and an interrupt back — about fifteen microseconds on Apple's hypervisor, most of which is not ours to spend. That is a second of the install before anything useful happens. Caching removes lookups; it cannot remove the create, the write and the release of a file that has to exist, and roughly 50,000 of those 65,000 requests are metadata that no data-path change can touch.
- **`npm ci` on APFS does not copy.** It clones from its own cache, which is nearly free. A container's cache lives on its own storage, on the other side of a device boundary, so the same install has to copy the bytes. The two commands are not doing the same amount of work, and no filesystem makes them.

### DAX is not the answer, and this is why

The obvious next move looks like virtio-fs's shared memory window: map file contents into guest physical memory and the data round trips disappear. It would make this case worse, and the guest kernel says so plainly.

`fs/fuse/dax.c` fixes the granularity at `FUSE_DAX_SHIFT 21` — every mapping is 2 MiB regardless of the file's size, `inarg.len = FUSE_DAX_SZ`, one `SETUPMAPPING` request each, drawn from a fixed pool of ranges. When the pool falls below a fifth, the kernel reclaims ten at a time and each reclaim is a `REMOVEMAPPING`.

Against 7,167 created files of a few kilobytes apiece: today that is about 13,672 `WRITE` requests. With DAX it becomes about 7,167 `SETUPMAPPING`s, each consuming a whole 2 MiB range — a one-gigabyte window holds 512 of them, so after the first 512 files every further one also drives reclaim, adding several thousand `REMOVEMAPPING`s and an `mmap`/`munmap` pair on our side for each. More round trips, not fewer, to replace the cheapest requests we make.

And `fuse_dax_write_iter` goes through `dax_iomap_rw`, which bypasses the page cache — the very thing that makes the read cases faster than macOS. DAX is the right tool for large files and mmap-heavy reads of big data, which is the shape this already wins at by a factor of five. It is the wrong tool for creating seven thousand small files.

### Tried and measured to be worth nothing

So that nobody spends an afternoon rediscovering it: `FUSE_WRITEBACK_CACHE` (no effect on either write case, and it would move the moment a container's output appears on the Mac from "as it is written" to "when the file is closed"), a larger worker pool, serving requests inline on the vCPU thread, and raising cache timeouts beyond thirty seconds.
