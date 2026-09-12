# lighter 0.5.3

Idle memory cleanup now preserves the file caches of running containers. A
quiet process can still need its executable and mapped files, including pages
charged to Docker's engine when it extracted an image. Previously, trimming
those pages after a short CPU-idle period could move their subsequent memory
charges into an already full container limit and stall its control process.

Proactive cache trimming now waits until the container hierarchy is empty and
idle, with passes after approximately three and eight seconds. Free-page
reporting and pressure-driven memory recovery continue. Live containers may
retain more useful cache at idle rather than paying to reload it.

Workload detection now reads the cgroup's hierarchical population flag. Nested
groups, including builders and their execution processes, count as live even
when the immediate parent has no direct processes. Missing or malformed
population information is treated conservatively as live work.

These are general memory-policy fixes. They do not add BuildKit-specific
handling or make oversized concurrent builds fit within a shared memory cap.
The Linux kernel remains **6.18.49** and the data epoch remains **1**.
