# lighter 0.6.0

## Idle page cache goes back to the Mac

A cached file nobody has read or written for an hour is memory the Mac could
have. Nothing aged memory while containers ran: a daily stack held twelve
hours of image-build cache, five gigabytes charged to a cgroup with no
processes, in macOS's swap file (defect observation, 2026-09-18).

The guest agent now runs an hourly pass over unmapped page cache, DAMON's
`pageout` with a per-page `young` check: a page untouched since the previous
pass is evicted, a page touched since is skipped and marked old for the next.
So a page goes at the first pass it reaches untouched since the pass before,
idle for at least an hour and at most two, decided per page from the kernel's
own access bits. Anonymous memory is never a candidate, and neither is a page
any process has mapped: a sleeping process keeps its heap, its text and its
mapped files. tmpfs pages idle for an hour go to the guest's zram, compressed,
and fault back in microseconds. What the pass frees returns through free page
reporting and through a balloon offer the host now takes with the virtio-mem
range in; a container being created, started, exec'd or built gets the whole
guest back in one motion, and a shrink lets the balloon go before the range
comes out.

The kernel gains DAMON (the sysfs interface and physical-address operations
only). The classic LRU and every reclaim path are as before. `lighter.idle_age`
on the guest command line shortens the hour; `0` leaves the pass off.

## Published ports answer on `localhost`

Make a published port answer on `localhost`. A Mac resolves `localhost` to
`::1` first, and Docker publishes a port in both families: its IPv6 mapping
reaches the container's IPv6 address, where a server that binds `0.0.0.0`,
which is most of them, does not listen. Docker refuses, lighter relayed the
refusal, and a dev server that answered on `127.0.0.1` did not answer on
`localhost`. Docker on Linux behaves the same way; this is a deliberate
difference from it.

A v6 publish Docker refuses is now retried by the guest agent on the same port
at the guest's IPv4 address, Docker's v4 mapping of that port, whether Docker
refuses at connect or, through its proxy, by accepting and hanging up before a
byte comes back. A server that answers over v6 is still reached over v6; only a
refusal triggers the retry, and the agent says so once per port in the machine
log. The proper fix in the application, binding `::`, stands regardless and
makes `localhost` work under every Docker.

The stream gate now publishes a server bound to `0.0.0.0` and reads it over
`::1`. Linux remains **6.18.49** with the 0.5.4 patches, and the data epoch
remains **1**.

## Linux 6.18.52

Linux moves from 6.18.49 to **6.18.52**, the current longterm point release,
with lighter's thirty patches rebased (one hunk of the vsock refill patch moved
beside an upstream use-after-free fix in the vsock remove path). Among the
fixes: the balloon driver stops using indirect descriptors on the reporting
queue, four vsock fixes including a use-after-free on device removal, a fuse
race between interrupt and resend, and a virtio-net TCP segment count overflow.
The data epoch remains **1**.

## Release artifacts

To be recorded at packaging.
