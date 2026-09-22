# lighter 0.7.3

A machine that is warm all day costs the Mac what its containers use, not
what they have cached. Found on 2026-09-21 on the Mac that runs the most
containers: 12.5 GiB held for 1.3 GiB of container memory, on a Mac that was
swapping, until a container was stopped.

## Idle cache goes back while containers run

Every way the guest gave cache back waited for a state a daily driver never
reaches: the trims for an empty container hierarchy, the balloon's offer for
a guest under a tenth of a core, the idle pass for an hour without a touch.
A day of image builds and container writes sat in the guest as cache, and
the Mac swapped to make room.

The agent now runs the loop Meta runs across its fleet (Senpai, in
"Transparent Memory Offloading", ASPLOS 2022): every six seconds it asks the
kernel to reclaim a sliver of the containers' file cache through
`memory.reclaim`, the kernel's own LRU chooses the pages, and the guest's
measured memory stall decides how large the sliver is. A working set that
starts refaulting raises the stall and brings the step to nothing within one
period; a step is megabytes, never the gigabyte at once that every earlier
attempt here paid for on every page. The Mac's need sets the gain: 1 with
memory to spare, 8 while it compresses, swaps, or is at Warn, 20 at
Critical, which halves a cache nothing refills in about seven minutes.
Contraction takes minutes, as it does in the datacenter; the pressure ramp
still answers in seconds. The agent's stall figure is the whole guest's,
net of the loop's own reclaim time, so the loop does not read its own work
as harm; the per-cgroup accounting stays off, as 0.7.1 left it
(`guest/agent/src/warm.rs`, `docs/warm-guest-memory-2026-09-21.md`).

`lighter doctor` has an `idle cache` row: how much came back, the Mac's
need, and how often the guest's own stall held the loop. `lighter.warm=0`
on the guest command line leaves it off.

## The host's word reaches the guest

Every request the host made of the agent over the control channel since
0.7.2, the reclaim asked ahead of each balloon step under pressure, was
written into the wrong end of a socket pair and never left the Mac. Each
timed out thirty seconds later at debug level, and no machine logged one
that worked. Found when the loop's gain went the same way and the agent's
own report said it had heard nothing. The line now goes through the device
both ways, and the warm benchmark case fails if the gain the host said is
not the gain the agent reports.

## The Neural Engine helper sleeps between frames

`lighter ane-host` read 43 to 53% of a core with Frigate sending five
frames a second: ONNX Runtime's worker threads spin after each task by
default, and a client that sends a frame every 200 ms never let them stop,
while the Neural Engine did every inference (1,449 of a three-second
profile's samples in the spin loop, 16 in CoreML). Spinning is off for both
of a session's pools and a CoreML session has one intra-op thread; a CPU
session keeps its width. The `m10` gate reads the VMM's CPU with a client
at 5 Hz: 2.6% of a core against 31% before.

## Also

- A `warm` benchmark case: a write-once writer at a third of a core, a
  reader of a tree on the guest's disk and one on the share, and a probe
  every five seconds, five minutes, reading the footprint, the cache, the
  re-read times and the host's CPU per second of the guest's. Not in the
  default set.
- `LIGHTER_RECLAIM_AHEAD=0` and `LIGHTER_WARM_GAIN=<n>` for the A/B.
- The `m6` gate's idle-pass boot runs with the ramp off: an overcommitted
  Mac is steered as at Warn whatever level it reports, and the ramp then
  reclaims the cold container's cache before the pass can.
- The README's tables are regenerated from the 0.7.3 records on both
  machines. One row moves for a reason worth knowing: the footprint's peak
  through an npm install reads 8821 MiB on the M5's 16 GiB guest, against
  the 3852 published for 0.7.2, which was measured on a 12 GiB guest. A
  single-zone guest holds the whole install as cache until the trims run
  fifteen seconds later; 0.7.2 reads the same at 16 GiB (8804 on the same
  night with the loop off). The readings that follow, 1605 and 1492 MiB,
  and the idle 604, are unchanged or better.
- Linux remains **6.18.52**; the data epoch remains **1**.
