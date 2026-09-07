# Filesystem event-loss recovery, 2026-09-07

The first release-source qualification on M5 failed the watch-latency case: its first request received no visible reply within 30 seconds. The numeric storage cases completed. M1 passed the same source qualification. The M5 failure remains an invalid qualification, not a slow timing sample.

Three focused storage-sequence repetitions subsequently completed, with first watch round trips of 153, 798 and 80 ms. A 20-round continuous test also passed. These successes did not explain the failure.

A stronger probe cached 1,000 distinct empty files before asking the host to update each. M1 completed all 1,000, with a 48 ms maximum. M5 failed on the first file: the host-side request and reply both contained `1`, but the guest continued reading an empty file for 30 seconds. A private diagnostic VMM logged 56,656 filesystem events during the run, including repeated flag value `3` (`MustScanSubDirs | UserDropped`). The reply's own path never appeared. The stream was also receiving delayed events from the preceding VM's package tree.

Apple requires a recursive rescan when event detail is lost; a notification naming a directory does not describe every changed descendant. Our callback ignored the flags and invalidated only the reported path and its parent. That left cached descendants valid for up to five minutes. Independently, our bounded notification queue discarded its oldest message on overflow, causing the same loss of information. [Apple's event-loss requirements](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/UsingtheFSEventsFramework/UsingtheFSEventsFramework.html).

The recovery path now:

- Turns a lost-detail callback into one complete reset for that share. A moved or removed watch root also permanently selects conservative timeouts until restart, including if feature negotiation races the root change.
- Replaces an overflowing notification backlog with a complete reset, followed by newer messages. The queue remains bounded; a reset subsumes earlier individual invalidations.
- Negotiates complete reset support separately from individual notifications. Older kernels use the existing conservative timeouts.
- Uses the guest's dentry epoch to withdraw positive and negative name caches. A request-version barrier expires attributes and open-file data, including responses that began before the reset. Contents are refreshed even when host writes preserve size and mtime.
- Invalidates existing mappings explicitly, refreshing size so a host-truncated mapping faults correctly. The kernel pins inodes while traversing its cache, following its existing cache-walk pattern, without allocating a list proportional to the cache size.

The signed hardware probe first mutates files from the VMM process itself, making FSEvents' IgnoreSelf deliberately hide the individual writes. It verifies that the guest still reads stale data for a full second, then injects either a reset or queue overflow. Both modes refresh an open descriptor, path reads, an existing mapping, an empty file, negative lookups, deleted and renamed names, and directory listings. The same-size/same-mtime fixture verifies data invalidation; a separate truncated mapping must receive SIGBUS. These checks passed on M5 and are now part of the filesystem gate. Unit tests exercise actual callback flag handling, permanent fallback after losing the watch root, reset ordering and backlog overflow.

Repeating the real M5 storage sequence followed by the 1,000-file probe completed all updates with the recovery path active. Reset warnings confirm that macOS again lost event detail. The maximum round trip was 1,429 ms under this overload. That is recovery evidence, not a claim of sub-100-ms visibility under all host conditions. A stalled event daemon can still delay notification delivery; the timeout backstop remains finite.

All initial failure logs and diagnostic runs are retained privately. Full release qualification and benchmark records must be refreshed after this runtime change; the earlier `38bfab4` measurements describe the preceding runtime.
