# The guest's memory, redesigned: 2026-09-20

A day's use of 0.7.1 on the M5 (48 GB, Nick's daily stack: Frigate, Home
Assistant, a Postgres, a dozen small services, and the deploys' builds)
found the memory layout the guest had carried since 0.4: two halves, a
balloon that lived in the wrong one, and transport that gave up when the
allocator did. This note is the investigation and the decisions; the
architecture doc has the design as it stands, the worklog the measurements
as they were made.

## The failure

A `docker buildx build` with a docker-container builder died at its export
with `no active session … context deadline exceeded`. Not the restart an
hour earlier: the build ran 19:15–19:19Z. The machine log had the guest
short of memory from 19:16:01Z with the balloon held at 4096 MiB; the
guest's vmstat had 478 thousand allocation stalls in its Normal zone and
35 thousand failed compactions; its kernel log had the vsock driver
refusing its 256 KiB receive buffers by the hundred for a minute; dockerd's
log had four container health checks that could not even start. BuildKit
looks its session up with a five-second deadline when the exporter runs,
and the session, carried over the docker socket, was gone.

## Two halves

The guest's RAM was a base of a quarter of the configured memory and a
virtio-mem range for the rest, the range onlined movable (patch 0026) so an
idle guest could unplug it and return its page arrays. `/proc/zoneinfo` on
the M5's 16 GiB guest: DMA 2968 MiB, Normal 5102 MiB, Movable 7938 MiB.
Kernel allocations, slab, vsock buffers, pipes, page tables, can use only
the first two. The balloon's units (patch 0014, 16 KiB compound pages so
the host can release them) were unmovable, and a balloon without migration
allocates with `GFP_HIGHUSER`: from the kernel-usable zones. A 4 GiB
balloon halved the kernel's pool. Free memory piled up in the movable zone
where the kernel could not use it; the balloon driver logged "cannot give
it" while `free` showed five gigabytes.

The range existed for one thing: unplugging when no container runs. The
architecture doc had already rejected shrinking under load, and this
machine always has containers. So on the machine that matters the range
never left, and the movable half's only effect was the cap.

## On demand

A privileged container holding tens of thousands of filled pipes (unmovable
pages, one 64 KiB buffer each) reproduces the state with containers
running: docker API calls from the Mac hung for 28 minutes on the M5, and
on the M1's 4 GiB home server every 4.8 GB `docker export` stream broke
with unexpected EOF. `scripts/repro-kernel-zones.sh` is that recipe with
timings: an export, a build through a docker-container builder with
`--load` (the M5's deploy), and plain API calls, before and under the
fragmenter and an anonymous hog.

## The stream's last step

strace on the agent during an export on the M1: `write(7, …, 233528) = -1
ENOMEM` on the vsock socket, 1.5 GB in. A 256 KiB write is one linear
packet (patch 0015), one allocation above the allocator's costly order,
which a fragmented guest refuses rather than compacts for. The agent's copy
loop took the error for the end of the stream. Guest patch 0033 sends a
refused packet shorter, halving down to a page (the header carries the
length and the caller already reads it back, 0018); the copy retries a
refused write for thirty seconds. With those alone, all three exports
survived the harness on the two-halves guest, at 33, 61 and 27 seconds,
with 700 receive-buffer fallbacks.

## One zone

Decided with Nick: the range goes. The guest boots with all of its RAM as
ordinary memory. Cost, measured on the M1 with the 0.7.1 kernel at 4 GiB:
boot unchanged (729–801 ms to Docker), idle a minute after a cold start
283 MiB with the range against 412 without, more than the page arrays alone
(48 MiB) because pages touched at boot lie scattered through four
gigabytes of pageblocks that reporting at the 2 MiB order never returns.
The 64 KiB reporting order is what the single zone's footprint is judged
with. On the same guest the harness ran 14, 16 and 14 seconds per export
under the same fragmenter and hog, API calls under 1.1 s, allocation stalls
31 thousand across the run against 207 thousand.

## A balloon that moves

Patch 0014 rewritten: units from a whole 2 MiB pageblock down to 16 KiB,
the largest the allocator has first, movable
(`CONFIG_BALLOON_COMPACTION`), migrated by compaction as compound folios.
The migration series feared at planning was mostly already in Linux 6.18:
`skip_isolation_on_order` lets compaction isolate a compound page smaller
than its target order, `isolate_movable_ops_page` takes the head, and
`alloc_migration_target` allocates the destination at the source's order.
The driver's part is to tell the host in units and keep its accounting in
each page's order. Patch 0034 registers free page reporting at order 4.

## The policy

A guest that reports short comes down the paced release ramp at any level
instead of being held; swap counts as overcommitment over an eighth of RAM
rather than over half of the swap files, which macOS sizes to what is in
use (the rule from the morning kept the M5's balloon pinned at 83% of a
file that had halved). The guest's signal is its memory stall time as well
as its free counts; every task stalled hurries the release. Before each
balloon step under pressure the host asks the agent to reclaim the same
amount from the containers' cgroup, so the kernel's LRU picks the victims
and the balloon pins pages that are already free.
