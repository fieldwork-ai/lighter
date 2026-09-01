# Benchmarks

```bash
benchmarks/latency.sh                        # one syscall at a time, 15s
REPEAT=3 benchmarks/latency.sh               # three boots, with the spread

benchmarks/run.sh --target native   --reps 3 # the workloads, 20 min each
benchmarks/run.sh --target lighter  --reps 3
benchmarks/run.sh --target orbstack --reps 3
python3 benchmarks/report.py
```

Reach for `latency.sh` first. It answers "did that change help" in fifteen
seconds and at a precision the workload cases cannot reach; see *How to
measure* below for which question each one can actually answer.

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
| `pnpm-install` | `pnpm install --frozen-lockfile` | the same tree, from a store on the runtime's own disk |
| `yarn-install` | `yarn install --frozen-lockfile` | the same again, and the third lockfile people have |
| `ripgrep` | reading every file in `node_modules` | opens and reads |
| `find-walk` | `find -type f` over `node_modules` | lookups and directory reads, no file opened |
| `copy-tree` | `cp -a node_modules node_modules_copy` | read and create together |
| `rm-rf` | deleting a package tree | unlinks and rmdirs, nothing else |
| `watch-latency` | a change on the host, seen in the guest | how quickly a cache can be corrected |

The three installs are not redundant. `pnpm` keeps its store on the runtime's
own disk and hard-links out of it, which it cannot do across a device boundary
— so through a share it copies, and the case measures what every containerised
`pnpm` user actually experiences rather than what `pnpm` is capable of.

`benchmarks/latency.sh` has its own cases, which are not workloads at all:
`create+close`, `create-parallel`, `stat-cached`, `stat-missing`, `write-4k`,
`write-chunked` and `unlink`, each timed one syscall at a time.

## How to measure, and what can be measured

Three instruments, and picking the wrong one is how an afternoon disappears.

| | resolves | costs | use it for |
|---|---|---|---|
| `benchmarks/latency.sh` | ~2 us | 15s, or 45s at `REPEAT=3` | did this change help |
| `run.sh --cases npm-install --reps 3` | ~5% | 6 min | did it land, and by how much |
| `run.sh` (whole suite) | ~5% | 20 min | the published table |

The workload cases cannot resolve small effects, and no number of repetitions
fixes that. Measured across twenty runs of `npm-install` under configurations
that turned out to be equivalent, the standard deviation is 269ms on a mean of
11,242 — 2.4%. Three repetitions resolve a 5.5% difference; ten resolve 3.0%;
resolving half a percent would take about three hundred and sixty. A difference
that small is not there to be found by running the same thing more times.

So: **if the effect you expect is smaller than five percent, do not use a
workload case at all.** Use `latency.sh`, which measures one syscall at a time
and resolves microseconds, and read the spread column before the number — a
change smaller than the spread has not been measured, however confident the
difference of two medians looks.

Two things `latency.sh` needs saying about it. Its own variation is
boot-to-boot rather than sample-to-sample, so `OPS` is already far past the
point of diminishing returns and `REPEAT` is the knob that matters; the first
boot is a warm-up and is discarded. And `create-parallel` is the only case that
issues its work concurrently, which makes it the only one that can see a lock
on either side of the boundary — a change that removes contention shows up in
every other case as nothing at all.

`GUEST_LOCAL=1` runs a case against the guest's own disk instead of the share.
It is not a comparison anybody ships; it is the decomposition, and it is what
separates "our filesystem is slow" from "the virtual machine is slow", which
look identical from outside and have different fixes.

## Where lighter stands, and why

The read cases are faster than macOS itself. That is not a trick: the guest's page cache answers without any round trip, and Linux's VFS is quicker than the one underneath it. It is only possible because the cache can be *corrected* — see `crates/lighter-fs/src/notify.rs` and the guest kernel patch — so the timeouts can be thirty seconds instead of a hundred milliseconds while a host edit still lands in single-digit milliseconds.

The write cases are not, and the plan's 85%-of-native target for `npm install` is not met. What that is actually made of, measured with the server's own opcode histogram rather than reasoned about:

One install is about 636,000 filesystem requests. Of those, 66,000 are creates
costing 39 microseconds apiece on the host — which is APFS making a file, and
is two thirds of all the host time in the run. The round trips on top of that
are about one to two microseconds each now, so they are no longer the story:
a missing `stat` costs 4.7 microseconds on the Mac and 6.6 through the share.
The story is that a package manager makes sixty-six thousand files and the
file system underneath charges full price for every one.

An earlier version of this section said the other half was `npm ci` cloning
from its cache with `clonefile`, which a container cannot do across a device
boundary. **That was wrong**, and it is left here as a correction rather than
quietly deleted: the npm cache holds gzip tarballs, not unpacked trees, so a
native install decompresses and writes every file exactly as ours does. The
advantage is only that it does it without a boundary in the way.

### Tried, measured, and not kept

So that nobody spends an afternoon rediscovering it. Each of these was measured
rather than reasoned about, and two of them were previously recorded here with
the wrong reason, which is its own lesson.

- **Write-back caching (`FUSE_WRITEBACK_CACHE`).** It does exactly what it
  advertises: on the shape a package manager writes in — one file opened once
  and filled eight kilobytes at a time — it collapses eight `WRITE` requests
  per file into one, 12,000 for 1,500 files becoming 1,506. It is slower
  anyway. The eight writes it removes cost 3.7 microseconds each, the one it
  leaves costs 7.7, and it adds two `SETATTR`s per file at 6.3 because the
  kernel takes ownership of size and mtime. End to end, 84 microseconds a file
  becomes 98, against 73 on the Mac. It was recorded here before as having "no
  effect", which was true only of the build it was measured on — that build had
  a virtqueue bug that writeback happened to trip.

- **More host worker threads.** A create costs 26 microseconds on one thread
  and 39 under sixteen, which looks like queueing worth avoiding. It is not:
  16, 8, 4 and 2 workers give 11.19s, 11.37s, 11.35s and 11.66s. The mean moves
  and the throughput does not.

- **The packed virtqueue layout (`VIRTIO_F_RING_PACKED`).** Neutral, twice, at
  11,108ms against 11,166ms. It is kept on because it is correct, tested, and
  where the ecosystem is going — but it is not an explanation for anybody's
  numbers, including OrbStack's.

- **Cache timeouts beyond thirty seconds.**

Two things that *are* load-bearing and read like they would not be, so that
nobody removes them for tidiness: serving a lone request inline on the vCPU
thread rather than handing it to a worker (turning it off costs 47% on
concurrent creates), and the guest spinning a hundred microseconds for its own
reply rather than sleeping (turning it off costs 85%).

### DAX is not the answer, and this is why

The obvious next move looks like virtio-fs's shared memory window: map file contents into guest physical memory and the data round trips disappear. It would make this case worse, and the guest kernel says so plainly.

`fs/fuse/dax.c` fixes the granularity at `FUSE_DAX_SHIFT 21` — every mapping is 2 MiB regardless of the file's size, `inarg.len = FUSE_DAX_SZ`, one `SETUPMAPPING` request each, drawn from a fixed pool of ranges. When the pool falls below a fifth, the kernel reclaims ten at a time and each reclaim is a `REMOVEMAPPING`.

Against the 66,213 created files of the current fixture, averaging fourteen kilobytes apiece: today that is about 157,000 `WRITE` requests, each costing 8 microseconds on the host. With DAX it becomes about 66,000 `SETUPMAPPING`s, each consuming a whole 2 MiB range whatever the file's size — a one-gigabyte window holds 512 of them, so after the first 512 files every further one also drives reclaim, adding tens of thousands of `REMOVEMAPPING`s and an `mmap`/`munmap` pair on our side for each. Fewer requests on paper; far more expensive ones, to replace the cheapest thing we do.

And `fuse_dax_write_iter` goes through `dax_iomap_rw`, which bypasses the page cache — the very thing that makes the read cases faster than macOS. DAX is the right tool for large files and mmap-heavy reads of big data, which is the shape this already wins at by a factor of five. It is the wrong tool for creating sixty-six thousand small files.
