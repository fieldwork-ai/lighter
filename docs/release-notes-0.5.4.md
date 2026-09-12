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

Packaged source: `7a8f40105488456e4356da76bf1d6b28f3ef6c34`.
Apple accepted notarization `97e92ef2-9c78-4102-9579-af4ca17b531e`;
the app ticket is stapled and Gatekeeper accepts the archive's app.

| Artifact | SHA256 |
| --- | --- |
| `lighter-0.5.4-arm64.tar.gz` | `bfceaf5c6257e4e8a9521ef60982baac2909a6d55739323b2a9f7a0039bd20c0` |
| `lighter-0.5.4-arm64` bootstrap | `9ac44e1e20c42bdb72285f90198fc13764dd6746954644f5c45d65217d41e4b7` |
| Guest kernel | `d6fda8264ae57151f11c9069966430468d510567547057a5cdd620e8c7be472f` |
| Guest root filesystem | `7fc3df5fda5607f08c4a3b1c81526436f2ff16b7f1a8656c69a42b786d5c5da8` |

Final release metadata may follow the packaged source; runtime, guest and build
inputs remain identical. Removing signatures from temporary executable copies
confirmed byte-for-byte agreement between the packaged binary and a rebuild.
