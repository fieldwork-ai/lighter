# 0.5.0 benchmark evidence

These records use runtime source `66897086c3702b75d79cb6fcfed7736842b80780`
and recording source `1b0caabf34fbe359f11cd47cc87ce45dc8583682`.
The [release report](../../../../benchmarks/RELEASE-0.5.0.md) explains the
statistics; [competitor methods](../../../../benchmarks/RELEASE-0.5.0-COMPETITORS.md)
explain incomplete attempts, instrumentation corrections and case exclusions.

- `m1/` and `m5/`: three complete Lighter suites, five fresh storage runs,
  ABBA comparisons, native storage inputs and continuous daemon observations.
- `*-baab/`: separately retained reversed-order follow-ups selected after
  observing the initial comparisons. They never replace the primary record.
- Runtime directories retain original CSV labels, stage logs, environment,
  guard observations and exit status. An original success status does not
  override a later setup-audit exclusion.
- `selection.json`: exact sources and per-case filters used for the README.
  The canonical copies live in `benchmarks/results/` and its M1 directory.
- `*-case-output/`: compressed measured-case output, raw/published hashes
  and diagnostic audits. The original protocol discarded successful memory
  output and warm-up/input-materialization output; it cannot retrospectively
  verify those statuses. Known affected observations are excluded.
- `*-docker-desktop-udp/`: raw JSON from separate UDP receiver verification.
- `*-final-observation*`: quiet windows after competitor measurements.
- `helpers/`: exact recording instrumentation identified by environment hashes.
  `inputs/` records package tools, image archive/configuration/manifest identity.

User-home paths are normalized and unrelated application inventories removed
from observer records. Compressed JSONL and case output use deterministic gzip.
Each manifest verifies published bytes; case manifests also retain raw hashes.
The root manifest covers this published collection, excluding itself.

Regenerate statistics and plots from this directory:

```sh
python3 scripts/records/analyze-release-050.py docs/records/0.5.0/benchmarks --output /tmp/lighter-050-analysis
# Plot dependencies used for these figures: matplotlib 3.11.1.
python3 scripts/records/plot-release-050.py docs/records/0.5.0/benchmarks --output /tmp/lighter-050-plots
python3 benchmarks/readme.py --write
python3 benchmarks/report.py
```

Run commands from the repository root. The analytical scripts read archived
CSV and compressed observations; they do not start a VM or rerun workloads.
The historical initial kind/socket diagnosis remains in adjacent directories.
