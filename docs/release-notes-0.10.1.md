# lighter 0.10.1

A machine with cooperative resources and a saved container starts again. On 0.10.0 it did not: every start of a cooperative machine that had any container in it, running or stopped, panicked about ten seconds in, and `lighter start` gave up after its timeout. A fixed machine, the default, was never affected.

## What went wrong

Before Docker restores saved containers, the guest's init waits until the memory the machine boots with is online, so a container does not start into a guest still bringing its RAM up. lighter tells init how much that is. On 0.10.0 it counted a cooperative machine's whole range, up to twice the Mac's memory, but that range is plugged in only as containers need it, and at boot only the 2 GiB base is online. init waited for memory it would never be given, gave up, and exited, and the kernel stops when init does. It now waits for the base.

The gates and the benchmark records start every machine with an empty engine, which is why nothing caught it. Gate m6c now creates a container, restarts the machine on the same disk and checks it boots; against 0.10.0's VMM that check fails.

If a cooperative machine will not start on 0.10.0, `lighter config --resources fixed` starts it until you upgrade.

## Also

- The guest kernel and root filesystem are 0.10.0's, byte for byte (kernel 9396cca3, rootfs c3290910): the fix is in the host. Linux remains **6.18.52**; the data epoch remains **1**.
