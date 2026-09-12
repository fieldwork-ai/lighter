# lighter 0.5.4

Fix a guest-kernel memory-pressure stall exposed by concurrent builds sharing a
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
