# Benchmarks

```bash
benchmarks/latency.sh                        # one syscall at a time, 15s
REPEAT=3 benchmarks/latency.sh               # three boots, with the spread

benchmarks/run.sh --target native   --reps 3 # the workloads, 20 min each
benchmarks/run.sh --target lighter  --reps 3
benchmarks/run.sh --target orbstack --reps 3
python3 benchmarks/report.py
```

Use `latency.sh` to investigate individual operations and the full workload suite
to measure application-level effects. Each has its own run-to-run variation;
see [repeatability](REPEATABILITY.md) before interpreting a difference.

Results go to `results/<target>.csv`; `report.py` turns them into `RESULTS.md`. Nothing in the report is hand-written, so a number nobody can reproduce cannot appear in it.

## What is being compared

Every target runs the same case scripts against the same fixture — a pinned `node_modules` tree — on the same machine. What differs is only how the directory reaches the process: on `native` it is the Mac's own disk, and everywhere else it is a bind mount through whatever that runtime uses for file sharing.

`native` is a whole-workload reference on macOS APFS. Native and container runs pin the same package tools, but use different operating systems and may execute different platform-specific package steps. A ratio of 100% does not establish zero filesystem overhead.

`--where guest` runs the cases on the runtime's own disk instead of the share, into `<target>-guest.csv`. `--arch amd64` runs them in the x86-64 build of the image (`--platform linux/amd64`, so under Rosetta on Apple silicon), into `<target>-amd64.csv`; the README's x86-64 table is those runs on the own disk. Two cases exist for that table's sake and run under either architecture: `cpu-sha256`, a gigabyte through `sha256sum` with no disk or network in it, and `container-start`, `docker run --rm alpine true` timed from the host.

## Timing protocol

**The timing loop runs inside the target.** It used to run outside, around a whole `docker run`, and that measured container startup: a metadata walk costing the filesystem 1,566 requests reported 550ms, of which about 450ms was Docker creating and destroying a container. The native target pays no such cost, so the comparison was not between two filesystems at all. Image loading and that container startup occur before the internal timing loop. Cache state can still change between operations and repetitions.

**Warm-up is attempted outside the timing loop.** Each target has its own package cache on its own storage. The 0.5.0 recording protocol attempted untimed package installations but ignored warm-up and per-repetition setup statuses. Three successful measured repetitions are required for each valid case; the first can still contain a colder access. Retained measured-case diagnostics do not retroactively verify the discarded warm-up output.

The current harness retains warm-up/setup failures and rejects the affected run.
By default, a focused package case warms only its own cache. To retain the full
suite's combined cache preparation while measuring only npm, use
`BENCH_EXTRA_WARM_CASES='pnpm-install yarn-install'` with `--cases npm-install`.
These extra cases run only during untimed preparation and are recorded in the
`.tree` metadata; they do not add measured repetitions.

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
| `watch-latency` | a host edit observed by polling inside the container | file visibility round trip, not fs.watch/inotify delivery |

The three installs are not redundant. `pnpm` keeps its store on the runtime's
own disk and hard-links out of it, which it cannot do across a device boundary
— so through a share it copies, and the case measures what every containerised
`pnpm` user actually experiences rather than what `pnpm` is capable of.

`benchmarks/latency.sh` has its own cases, which are not workloads at all:
`create+close`, `create-parallel`, `stat-cached`, `stat-missing`, `write-4k`,
`write-chunked` and `unlink`, each timed one syscall at a time.

## How to measure, and what can be measured

Release records use the versions in `toolchain.json` for both native and
container workloads. Prepare the private native tools with
`bash scripts/records/prepare-benchmark-tools.sh`, then set
`BENCH_TOOLS_PATH="$PWD/.logs/050/tools/native/bin"` and
`BENCH_REQUIRE_PINNED_TOOLS=1`. This preserves the Mac's globally installed tools.
Controlled release runs also load identical prebuilt arm64/amd64 image archives
through `LIGHTER_BENCH_IMAGE_DIR`; each archive hash and loaded image ID is
verified, and the native tool versions and image ID accompany each CSV.

Use the workload-specific, fresh-run CVs in [REPEATABILITY.md](REPEATABILITY.md),
not a universal five-percent cutoff. The 0.5.0 record runs three full suites on
each host, five additional fresh storage suites, and a separate old/new/new/old
comparison. Each storage case contributes the median of three ordered timings.
The between-run CV uses the sample standard deviation divided by the mean of
those medians. It is descriptive, not a significance test or regression gate.

`latency.sh` narrows an investigation to individual syscalls. Repeat across fresh
boots and retain ordered repetitions: many samples from one boot do not measure
between-boot variation. Its guest-disk control can help locate an effect, but
shared and guest filesystems have different bottlenecks; neither path cancels
arbitrary background interference.

Release timing requires six consecutive aggregate host-CPU samples at most 5%,
ten seconds apart. The VM guard samples throughout and rejects unrelated VMs.
The selected competitor is attributed to its exact executable or private
instance, including VM restarts. The observer records filesystem-daemon CPU and
memory without restarting it. All failed attempts remain separate from valid
performance records. Quiet preflight does not guarantee a perfectly idle host
throughout a workload, so the continuous observations matter too.

Alternate version order when checking an apparent change, preserve the original
record, and disclose any follow-up selected after seeing its result. Alternation
reduces some order effects; it cannot ensure interference affects both versions
equally. The [0.5.0 report](RELEASE-0.5.0.md) records the complete comparison and
its limits, including the unusually variable M1 read case.

## Historical filesystem investigations

The measurements and design experiments below predate the 0.5.0 release record.
They explain earlier implementation decisions. The former 85%-of-native install
target is historical, and these samples are not current performance claims.
Use the regenerated README tables and release record for current results.

In these historical samples, the read cases were faster than their native macOS reference. That is not a trick: the guest's page cache answers without any round trip, and Linux's VFS is quicker than the one underneath it. It is only possible because the cache can be *corrected* — see `crates/lighter-fs/src/notify.rs` and the guest kernel patch — so the timeouts can be thirty seconds instead of a hundred milliseconds while a host edit still lands in single-digit milliseconds.

The historical write samples did not meet the earlier 85%-of-native target for `npm install`. What that is actually made of, measured with the server's own opcode histogram rather than reasoned about:

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
