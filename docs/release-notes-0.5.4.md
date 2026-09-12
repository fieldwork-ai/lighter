# lighter 0.5.4

Fix a byte-stream corruption in the kernel join of a container's TCP
connection to the host. When a retransmitted segment began inside bytes the
guest had already received, Linux queued it whole, as it does for every reader
that skips by sequence number, and the sockmap reader did not skip: the
overlap went to the host twice. The 0.5.4 release gate caught it as a quarter
gigabyte out of a container arriving 5,792 bytes long, four segments repeated
in place, after the same check had passed on every earlier release. Every
release since the join, 0.4.1 through 0.5.3, is affected; a raw-socket client
that sends sixteen bytes and then sixteen more from eight bytes back gets
thirty-two bytes through the 0.5.3 guest and twenty-four through this one. It
needs a retransmission that crosses the acknowledgement for part of itself, so
it is rare and shows as a checksum mismatch on a bulk transfer, never on
copying without the join (`LIGHTER_SOCKMAP=0`).

The reader now keeps its own place in the stream and trims each segment to
what lies beyond it (kernel patch 0030). The stream gate sends the overlapping
segments on every run, and CI drives the patched function itself.

Also fix a guest-kernel memory-pressure stall exposed by concurrent builds sharing a
full memory limit. Speculative file readahead could enter direct reclaim while
servicing a page fault, leaving BuildKit and its control requests stuck instead
of making progress or reporting an out-of-memory failure.

Readahead now avoids direct reclaim. Ordinary demand reads and page faults keep
their existing reclaim and out-of-memory behavior. This is a general kernel
correction; it adds no BuildKit-specific limits or scheduling policy.

Oversized concurrent builds can still exhaust their shared memory limit. In the
reproduction, one or both builds could fail with an explicit OOM error; the
builder recovered and accepted subsequent work without restarting. A transient
4.1-second control request was observed, so this is not a guarantee of immediate
responses under memory pressure.

The [investigation and qualification](buildkit-memory-pressure.md) distinguish
this fix from the idle-cache policy corrections shipped in 0.5.3. Linux remains
**6.18.49**, with the readahead correction applied, and the data epoch remains
**1**.

## Release artifacts

Packaged source: `ab2cabb` (`ab2cabb3` on the release branch; the stream
fix). A first candidate from `7a8f40105488456e4356da76bf1d6b28f3ef6c34`,
notarized as `97e92ef2-9c78-4102-9579-af4ca17b531e`, was withdrawn unpublished
when its stream gate failed; only the kernel differs.
Apple accepted notarization `30e46b48-b446-44b8-bf5b-908c3a7afc1e`;
the app ticket is stapled and Gatekeeper accepts the archive's app.

| Artifact | SHA256 |
| --- | --- |
| `lighter-0.5.4-arm64.tar.gz` | `f9533404042f398617980171ab3f499a454043574c5118dda7488bb3a26cb071` |
| `lighter-0.5.4-arm64` bootstrap | `9fbdb218cc8ca47f3b31e4dfe5f3b3d7461fb3038de75d3ba0ee4cb81dd498dd` |
| Guest kernel | `33c72dd4987331679097b0fa360f57beb621c4ee10ec111eb3064eeaaf517a97` |
| Guest root filesystem | `7fc3df5fda5607f08c4a3b1c81526436f2ff16b7f1a8656c69a42b786d5c5da8` |

Qualification of this exact archive: on the dedicated M1, formatting, clippy,
360 workspace and 24 hypervisor tests, all twelve hardware gates (the stream
gate now sends the overlapping segments on every run), a fresh-install smoke,
a real 0.4.1 upgrade preserving a running container and volume, staged
upgrades with interrupted-transaction recovery, tamper rejection, rollback and
the login service, kind restart persistence, and a Homebrew upgrade from the
public 0.5.3 with the installed binary byte-equal to the bootstrap. On the M5,
the same source checks, the raw-socket overlap reproducer, the stream gate,
smoke, migration, the idle-cache memory policy and the BuildKit workflow above.
The benchmark suite was not repeated for this rebuild.

Final release metadata may follow the packaged source; runtime, guest and build
inputs remain identical. Removing signatures from temporary executable copies
confirmed byte-for-byte agreement between the packaged binary and a rebuild.
