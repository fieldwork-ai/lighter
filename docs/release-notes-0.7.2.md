# lighter 0.7.2

One memory zone, a balloon that moves, and streams that survive a guest
short of memory. Found on 2026-09-20 within an hour of 0.7.1, on the Mac
that runs the most containers.

## The guest's memory is one zone

Until 0.7.1 the guest booted with a quarter of its RAM and plugged the rest
in as a virtio-mem range onlined movable, so an idle guest could unplug it
again. Containers always run in this machine, the range never left, and the
movable half only capped what the kernel could use: the balloon's unmovable
16 KiB units came from the kernel-usable half, and with 4 GiB of them a
16 GiB guest had four gigabytes for its kernel, its vsock buffers and its
slab. A build's node processes then starved it: 478 thousand allocation
stalls, the vsock driver refusing its receive buffers by the hundred,
health checks that could not start, and the build's BuildKit session gone
by the time its exporter looked for it.

The guest is now one zone. virtio-mem is gone, with its plug-before-a-
container check and its shrink policy, and nothing can halve what the
kernel has. The price, chosen with its number known, is the page array for
memory that never leaves: 1.56% of RAM at idle, about 130 MiB on a 4 GiB
guest and 290 on a 12 GiB one, nothing while containers run
(`docs/guest-memory-2026-09-20.md`). The kernel's 64 MiB swiotlb, which no
device here uses, is no longer set aside (`swiotlb=noforce`), and the page
array is initialised across the vCPUs rather than on one core before init.

## The balloon inflates in whole pageblocks and its pages move

The balloon's units are compound pages from a whole 2 MiB pageblock down
to 16 KiB, the largest the allocator has first, so a balloon of gigabytes
is thousands of whole blocks rather than hundreds of thousands of islands,
and what the guest keeps stays compactable. The units are movable pages
(`CONFIG_BALLOON_COMPACTION`): allocated as movable, so the balloon never
takes the pageblocks the kernel's own allocations live in, and migrated by
compaction as compound folios, the driver telling the host the new unit
before the old. The kernel's default free page reporting order, for the
moments before the agent sets its own, is four host pages rather than a
2 MiB pageblock (guest patches 0014 and 0034).

## Streams survive a guest short of memory

A 256 KiB vsock packet was one linear allocation above the allocator's
costly order, which a fragmented guest refuses rather than compacts for;
the agent's copy loop for the docker socket took the refusal for the end
of the stream (traced: `write(…) = -1 ENOMEM` mid-export, the client seeing
unexpected EOF). A refused packet is now sent shorter, halving down to a
page (guest patch 0033), and the copy retries a refused write for up to
thirty seconds instead of ending the stream.

## The policy listens to the guest

- A guest that reports short comes down the paced release ramp at any
  level, a 32nd of RAM a second and never more than half of what the Mac
  has free, instead of being held where it is.
- The guest's "short" is now its memory stall time (pressure stall
  information) as well as its free counts, and a guest whose every task is
  stalled comes down twice as fast.
- Before each balloon step under pressure the host asks the agent to
  reclaim the same amount from the containers' cgroup, coldest cache first;
  the balloon then pins pages that are already free.
- Swap counts as overcommitment over an eighth of the Mac's RAM, not over
  half of the swap files, which macOS sizes to what is in use.

## Also

- `scripts/repro-kernel-zones.sh` reproduces the starvation on demand with
  containers running (unmovable pages fill the guest; a 4.8 GB export, a
  docker-container build and API calls are timed against it); `m6b` gates
  the balloon's units and their migration. Linux remains **6.18.52**; the
  data epoch remains **1**.
