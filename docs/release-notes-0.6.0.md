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
reporting, fed by a compaction pass, within seconds; nothing on the host
changes.

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

Packaged source: `9e00a99` on `release/0.6.0` (the idle pass, Linux 6.18.52
and the version bump). Apple accepted notarization
`78ec4c63-53cc-4bc1-9443-a4dd81efc715`; the app ticket is stapled and
Gatekeeper accepts the archive's app.

| Artifact | SHA256 |
| --- | --- |
| `lighter-0.6.0-arm64.tar.gz` | `ca39d1e45b4a4597c271684abc29f718f769d139ead7f3f8feacbbbb1ff8a6c6` |
| `lighter-0.6.0-arm64` bootstrap | `e26a23ee20b696f3f3f22404eee33099dfc8d984072b3fbe097c5430fd86643b` |
| Guest kernel (Linux 6.18.52) | `27fb18843a7edcb89e36f76e4acdc2ae9615c77bf314e1b4baa195134d6dc92c` |
| Guest root filesystem | `eb6490a09f36bbfa981396bb515ae8640a668432f1f6fdee04738bbf1afc263b` |

The kernel is new: 6.18.52 with DAMON, built on the release commit. The root
filesystem differs from 0.5.6's only in the guest agent: the same 102 packages
at the same versions, and the init scripts, dockerd, containerd and runc are
byte-identical. Final release metadata may follow the packaged source;
runtime, guest and build inputs remain identical.
