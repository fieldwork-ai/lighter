# Performance records

- [0.5.0](0.5.0/): full benchmark and competitor qualification records.
- [0.5.1 hybrid](0.5.1/hybrid/): selected full suites and focused follow-ups;
  interpretation is in [the performance report](../RELEASE-0.5.1.md).
- [Superseded 0.5.1 quarter-RAM candidate](0.5.1/quarter/): historical evidence,
  excluded from the final hybrid qualification.
- [Selected composite inputs and metadata](0.5.1/selection/): retained inputs
  that differ from a single raw run, such as competitor case exclusions.

The report manifests in `../results/selection.json` and
`../results/machines/m1/selection.json` point to these records directly.
Raw files are stored once; equal outcomes from independent runs remain
separate observations. Recorded paths inside logs/environment files describe
where the run happened, not the present repository layout.

Correctness and signed-release evidence remain in
[docs/records](../../docs/records/). Design/prototype experiments stay with
that engineering evidence; full release benchmark data belongs here.

Evidence manifests can reference another record directory for an input or
check reused verbatim. Those relative entries retain the original SHA-256;
they are not additional measurements. Per-run status, image fingerprints and
independent outcomes remain with each run even when their bytes match.
