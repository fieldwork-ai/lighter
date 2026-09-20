# lighter 0.7.1

Three things a day's use of 0.7.0 found, each measured on the Mac that found
it, each fixed at its cause.

## A Mac short of memory no longer stalls the machine

The balloon policy asked the guest for a quarter of its memory when macOS
reported Warn, and handed it all back in one step the moment macOS read
Normal, which on an overcommitted Mac it does for seconds at a time between
Warns. On 2026-09-20 that put 4 GiB back into a Mac holding 53 GB of
compressed pages, and the Mac paged the guest for thirty seconds: an RCU
stall inside, twenty-five stream connects refused, a build's BuildKit session
lost. Now a release comes down a ramp, a 32nd of RAM a second and never more
than half of what the Mac has free per second; a Warn is remembered for a
minute; and a compressor holding a quarter of RAM, or swap half used, counts
as pressure whatever level the Mac reports. The guest agent's connects to the
host retry for forty-five seconds before a stream is refused, so a Mac that is
paging its guest pauses new streams instead of losing them
(`docs/architecture.md`, memory).

## Containers reach devices on your local network

macOS Local Network privacy denies a bundled app unicast to other devices on
the Mac's network until the user allows it, and lighter's machine is one since
0.5.4; the gateway and the internet are exempt, shell tools are not subject,
and without the usage string in the bundle there was no dialog, so a camera,
a printer or a NAS simply stopped answering containers with nothing logged
anywhere. The bundle now declares `NSLocalNetworkUsageDescription`, the
machine asks at start with one mDNS query so the dialog appears once and up
front, and `lighter doctor` gains a `local network` row that has the running
machine connect to a device on the network that is not the gateway and says
allowed, denied with the setting to change, or untested and why
(`docs/local-network-2026-09-20.md`).

## Smaller things

- The Neural Engine helper says, at info, why a client's stream ended: the
  frame it was in, the sessions it held, and the error.
- The poll window's inputs are readable while a model runs:
  `/sys/module/idle/parameters/{traffic,limits,judged}` in the guest.
