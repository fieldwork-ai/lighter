# Worklog: what was tried, what it did

One row per experiment, appended as they happen, so nobody reruns one by accident. Keep rows to one line. Numbers are the ones that decided it, on the machine named; "M5" is the M5 Pro, "M1" the 8 GB M1. Verdicts: **kept**, **dropped**, **ruled out** (a hypothesis, not a change), **open**.

| date | area | tried | result | verdict |
|---|---|---|---|---|
| 2026-09-01 | own disk | mmap the disk image instead of preadv/pwritev (all variants) | slower on every case | dropped |
| 2026-09-01 | own disk | rq_affinity changes, dsb barrier, level interrupt line alone | no change | ruled out |
| 2026-09-02 | own disk | host poller on the block queue alone | wake latency eats it; M1 suite flat | dropped alone, kept with guest poll |
| 2026-09-02 | own disk | guest virtio-blk poll-after-submit (patch 0008), `virtio_blk.poll_usecs` | fio qd1 reads 40k → 128k, writes 35k → 208k; 2 interrupts per run | kept |
| 2026-09-02 | own disk | both sides polling: guest poll + host watcher on block queues | fio reads 124k/415k, writes 316k/290k (qd1/qd8) | kept |
| 2026-09-02 | own disk | bounce buffer for reads instead of preadv into guest pages | 130k vs 124k, noise | dropped |
| 2026-09-02 | own disk | fio working-set size 16M..2G | 16M still 6.4 µs/read; not a cache miss story | ruled out |
| 2026-09-02 | own disk | one virtio-blk queue per vCPU (`VIRTIO_BLK_F_MQ`) | read completions no longer via ksoftirqd; fio reads 124k → 438k qd1 | kept |
| 2026-09-02 | guest kernel | lean config: no PAC/BTI/MTE, no stack zero-init, stack-protector regular, no audit, no IRQ accounting, voluntary preempt | stat/open ~8% faster on M1, nothing on installs | dropped |
| 2026-09-02 | guest kernel | `rodata=on` | no change | ruled out |
| 2026-09-02 | memory | free-page reporting withheld | fresh-memory touch 3x faster (60 → 15 ms per 512 MiB) but OrbStack pays the same; installs unchanged | open (lever past parity) |
| 2026-09-02 | memory | guest 6 GB instead of 4 GB on the M1 | worse (host is 8 GB) | ruled out |
| 2026-09-02 | scheduling | `idle=poll` in the guest | no change: vCPU wake latency is not the gap | ruled out |
| 2026-09-02 | scheduling | QoS classes on vCPU threads, static (all interactive) | flat on M5 and M1 | dropped |
| 2026-09-02 | scheduling | QoS override on busy vCPUs only, sampled every 25 ms | flat on M5; npm slightly worse on M1 | dropped |
| 2026-09-02 | scheduling | 4 vCPUs instead of 8 on the M1 | yarn −4%, npm −7%, sys −1 s | open (cause unknown; not QoS) |
| 2026-09-02 | filesystem | ext4 without metadata checksums | mkdir+rmdir −40%, unlink −35% on M1; installs unchanged | dropped (btrfs is the default; ext4 keeps the standard format) |
| 2026-09-02 | filesystem | ext4 without a journal | no change | ruled out |
| 2026-09-02 | filesystem | ext4 without dir_index | readdir −40% but file creation 10x slower | ruled out |
| 2026-09-02 | filesystem | XFS data disk (reflinks) | yarn 10.4 → 8.5 s on M1 (system time halved); rm-rf 350 → 620 ms | superseded by btrfs |
| 2026-09-02 | filesystem | XFS log/agcount tuning (`logbsize=256k,logbufs=8`, `agcount=8`) | no change | ruled out |
| 2026-09-02 | filesystem | btrfs data disk, nodatacow, `-m single` | yarn 8.3 s on M1 (OrbStack 7.8); copy-tree at parity on M1 | kept (decision) |
| 2026-09-02 | filesystem | VFS clone-size threshold (`fs.clone_min_bytes`) | a clone beats a copy at every size on btrfs once files are clean (1 KiB: 24 vs 50 ms/2000); the "small clones cost 2x" reading was dirty pages being flushed | dropped |
| 2026-09-02 | filesystem | btrfs `ssd` mount option | no change | ruled out |
| 2026-09-02 | filesystem | btrfs inline completion of checksum-free reads (patch 0009) | fio reads on btrfs 39k → 415k qd1 | kept |
| 2026-09-02 | filesystem | same for writes on 6.18 (where btrfs punts writes to the workqueue too) | fio writes 32k → 119k qd1 | kept |
| 2026-09-02 | memory | THP `madvise` instead of `always` | no change | ruled out |
| 2026-09-02 | writeback | `vm.dirty_writeback_centisecs=1500` (OrbStack's) | no change | ruled out |
| 2026-09-02 | own disk | hole-filling cost of a fresh sparse image | 1 GB fresh vs overwrite equal; copy-tree ×5 stable at 2.8 s on M1 | ruled out |
| 2026-09-02 | guest kernel | 6.12.51 → 6.18.49 (nine patches re-derived; one now upstream) | install suite identical on M1; fio identical on the quiet M1 | kept (LTS) |
| 2026-09-02 | guest kernel | 7.2.3 measured | suite identical to 6.18; rm-rf 2.5x slower (7.x btrfs defaults `discard=async`); completion patch hangs it | ruled out |
| 2026-09-02 | virtio-blk | poll only for synchronous requests (writeback's async writes get one look) | wall time unchanged; a third of a CPU no longer burned under a tree copy | kept |
| 2026-09-02 | VMM | exit counting (`LIGHTER_EXIT_STATS=1`) | yarn is 3k–10k exits/s, all virtio kicks; not the gap | kept (diagnostic) |
| 2026-09-02 | share | pnpm on the share, guest profiles | ours 68% idle with a kick per request; OrbStack spins in `virtio_fs_request_complete` and is answered in time — fs-server latency | open (share milestone) |
| 2026-09-02 | own disk | copy-tree in a persistent VM (`.logs/loop.sh`), counters per copy | our copy writes the full 1 GB to disk while `cp` runs (66k writes, 2.9 s system); OrbStack's writes 80 MB and 1 s system — writeback is forced mid-copy on our side | open (cause being found) |
| 2026-09-02 | memory | guest 16 GB instead of 8 GB on the M5 (OrbStack's guest is 16 GB) | copy unchanged: still 1 GB written during the copy | ruled out (not the dirty threshold) |
| 2026-09-02 | scheduling | vCPU count 4 vs 8 on the M1 | yarn −4%, npm −7% (see above); QoS is not why | open |
| 2026-09-02 | filesystem | btrfs default mounted an existing ext4 disk with btrfs options (`nodatacow`) | mount failed, dockerd started on the 2 GB root and died — the daily driver lost Docker; fixed by mounting by the type blkid reports, formatting by the default | fixed (trap) |
| 2026-09-02 | own disk | per-device counters around a copy on both guests | OrbStack writes the full 1 GB during the copy too (55k writes, 20 KB each) — the gap is per-write cost: 3.0 s system vs 1.6 for the same writes | open (completion path) |
| 2026-09-02 | virtio-blk | poll for asynchronous (writeback) requests too (`virtio_blk.poll_async`, runtime switch) | copy 2.0–2.1 s vs 2.2–2.7 without; system time 2.9 s either way | kept on by default (small wall gain), not the cost |
| 2026-09-02 | own disk | copy at 1, 2 and 8 vCPUs, and 8 with `idle=poll` | wall 1.7–2.2 s at every count; system time 1.4–1.7 s at 1–2 vCPUs vs 2.8–3.0 at 8 — btrfs workers waking each other across idle vCPUs cost ~1.3 s of CPU off the critical path; `idle=poll` no change | open (worker pool / wakeups) |
| 2026-09-02 | filesystem | btrfs `thread_pool=2` / `=1` mount option | copy system time 2.9 → 2.5 / 2.2 s, wall unchanged | open (decide after the reclaim fix) |
| 2026-09-02 | filesystem | why the copy is written back mid-copy: call chain is `btrfs_preempt_reclaim_metadata_space → flush_space → btrfs_start_delalloc_roots` | the fs has one 256 MB metadata chunk (btrfs's size for disks under ~50 GiB; the bench disk is 32 GiB) and every small file's reservation trips preemptive reclaim; OrbStack's fs has a 1 GB chunk with 700 MB free | open (fix in flight: bigger metadata chunk) |
| 2026-09-03 | filesystem | btrfs preemptive metadata reclaim off (`btrfs.preemptive_reclaim=0`, patch 0010) | copy system time 2.9–3.1 → 2.2–2.4 s, wall unchanged; the copy is still written back in full by a second path | kept as a switch; not the whole story |
