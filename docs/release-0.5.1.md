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

The focused M5 copy slowdown did not reproduce. Deletion's retained sample
measured 3,015 versus 2,830 ms (+6.5% by median), but its version effect remains
inconclusive against substantial host/workload variation. The full hybrid suite
spans 2,642–3,662 ms for deletion. Noise is plausible; a small version effect
has not been isolated or excluded. This is not labelled an established
regression or an accepted performance cost. The original full suite remains
the primary, and later informal repetitions are excluded.

Final hybrid archives still need to be built, signed, notarized and checked
through fresh installation, migration, rollback, kind and Homebrew. Both PRs
and the GitHub release remain drafts. No 0.5.1 has been published.

The previous [quarter-RAM qualification](release-0.5.1-quarter.md) and
[its artifacts](release-0.5.1-quarter-artifacts.json) are retained as historical
evidence. Those archives do not qualify the selected hybrid runtime.
