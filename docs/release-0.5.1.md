# 0.5.1 release qualification

The selected runtime is `b78dfeb3063e3ad7af0660050a766032c36f23c0`.
One complete benchmark suite on each of M5 and M1 now passes, with three
repetitions per timed case. All twelve M1 hardware gates also pass.

M5 measures **517 ms to Docker / 664 ms to the first container**, at 8 vCPUs
and 16 GiB RAM. M1 measures **720 / 905 ms**, at 8 vCPUs and 4 GiB RAM.
These are Node-based full-suite medians, separate from the Python-based
startup experiments. [Every observation and historical comparison](../benchmarks/RELEASE-0.5.1.md)
and [the design, trade-offs and functional checks](demand-memory-2026-09-08.md)
remain available.

A focused M5 old/new copy/deletion comparison is in progress because those
full-suite medians exceed the historical 0.5.0 record. The fresh full suite
remains the selected primary; interrupted attempts and shared-host diagnostics
are retained separately. The earlier M1 focused package follow-ups did not
reproduce a consistent slowdown.

Final hybrid archives still need to be built, signed, notarized and checked
through fresh installation, migration, rollback, kind and Homebrew. Both PRs
and the GitHub release remain drafts. No 0.5.1 has been published.

The previous [quarter-RAM qualification](release-0.5.1-quarter.md) and
[its artifacts](release-0.5.1-quarter-artifacts.json) are retained as historical
evidence. Those archives do not qualify the selected hybrid runtime.
