# lighter 0.5.1

Linux now starts while Lighter prepares the remaining guest RAM in the
background. This shortens the path to Docker readiness while preserving the
independent 16 KiB backing objects used for correct memory accounting and
reclamation. First-container completion is measured separately, including its
remaining memory preparation wait.

On the controlled M1 signed-archive comparison (8 vCPUs, 4 GiB), Docker
readiness improves from 716 to 536 ms (25% less elapsed time), and the first
container finishes in 715 ms instead of 901 ms (21% less). Saved-container
startup also improves. These are measured configurations, not universal bounds.

Container operations wait for host backing preparation before requesting the
full guest memory target. When saved containers exist, guest startup waits for
all configured memory blocks before starting Docker, protecting workloads
restored by restart policy. Preparation respects memory-pressure targets and
cancels waiting requests on shutdown. Configured guest RAM remains distinct
from total macOS process overhead.

Release archives now use portable regular tar entries for the root filesystem.
Packaging checks the archive through the real installer, catching sparse-entry
formats that macOS `tar` accepts but the application's extractor cannot install.

The Linux kernel remains 6.18.49. Existing kind support and explicit update
ownership remain in place. The full README comparison remains the 0.5.0 M5
record; the shared M5 was not used for a new full performance comparison.

[Qualification and artifact details](release-0.5.1.md) include the M1 workload
record, separate startup measurements, limitations and exact release hashes.
