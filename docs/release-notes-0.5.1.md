# lighter 0.5.1

Cold startup no longer waits for host backing objects for all configured RAM.
Linux starts with demand-safe memory while one worker prepares the remaining
backing. Docker can serve requests before that work finishes. Once a region
is ready, memory access bypasses preparation checks and reclamation returns
to the whole-range path.

Independent 16 KiB ownership preserves accurate memory accounting and physical
reclamation. Caching the Mach task port also removes an unnecessary kernel
trap per page. The selected hybrid balances startup latency with completed
preparation and efficient reclamation; it does not eliminate all contention
with guest boot.

Release qualification includes one full benchmark suite on each of M1 and
M5, with three repetitions per timed case. The README uses the new M5 Lighter
measurements alongside the retained 0.5.0-release competitor measurements.
See the performance report for all observations, workload differences and
focused follow-ups; there is no blanket workload speedup claim. A focused host-share deletion
sample was 6.5% higher than 0.5.0; substantial run variation leaves that small
version difference inconclusive.

The benchmark harness now retains Docker startup failures and stops its
private VM even when a warm-up fails. Release archives use portable regular
tar entries and are checked through the actual installer.

The Linux kernel remains **6.18.49**. Existing Docker, amd64/Rosetta, kind and
installation-ownership behaviour remain supported. Updates preserve VM
configuration and container data; Homebrew installations remain managed by
Homebrew.

[Qualification and artifact details](release-0.5.1.md) ·
[Performance measurements](../benchmarks/RELEASE-0.5.1.md)
