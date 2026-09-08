# 0.5.1 benchmark evidence

Runtime source: `67d787b5910e0dd68b7564d4716fe3b7b0126d1d`.

- `m1-full`: first valid full suite, attempt 2; primary by execution order.
- `m1-full-run2`, `m1-full-run3`: two further complete suites without a daemon reset.
- `m1-abba`: matched 0.5.0 / 0.5.1 / 0.5.1 / 0.5.0 storage follow-up, selected after the first suite differed from the historical baseline. It ran between full suites one and two.
- `m1-signed-boot`: the published 0.5.0 and final notarized 0.5.1 archives, separate image-only and saved-stopped-container ABBA comparisons; five starts per arm. No measured repetition was discarded.
- `m1-post-full`: ten-minute daemon observation after the three suites, before signed-archive tests. The same fseventsd process remained running throughout.
- `*-case-output`: all 88 measured-case output files from the full and storage runs. Warm-up/setup exit statuses were not retained by the inherited protocol and cannot be reconstructed from clean measured-case diagnostics.
- `helpers`: exact private drivers, with user-specific paths normalized. The tracked `scripts/records/record-boot.py`, `scripts/records/monitor-host.py` and `benchmarks/guard.py` at the runtime source supply the underlying protocol.

Full-suite attempt 1 stopped before measurement because the M1 checkout lacked the published v0.5.0 tag. Its failure record is retained separately. All subsequent valid complete suites are included; the first remains primary. The original historical comparison is retained in `historical-comparison.json`; it prompted the matched follow-up and is not the final release speed claim.

The full suite starts the checkout CLI with `guest/out` and a development app, using Node wall-clock timestamps. Signed boot tests start the shipped Developer ID app and use Python monotonic timestamps. Their absolute timings must not be pooled. Configurations and hashes are recorded per run; the final signed archive hash is d362b9e7b6ec275e9d16b633968716dda9f40f35f76b43631b1145a5b82f81ea.

Quiet guards require six aggregate CPU readings at most 5%, ten seconds apart, and reject competing VMs throughout. Readiness probes include command execution and a 50 ms interval after failed attempts. The M5 remained shared and was not used for full release benchmarks. Unrelated process inventories are omitted, user paths normalized, and JSONL/log evidence compressed where useful. Manifests hash the archived representation.

Regenerate and validate all tables from the repository root:

```sh
python3 scripts/records/analyze-release-051.py docs/records/0.5.1/benchmarks --output /tmp/lighter-051-analysis
```

See [the generated report](../../../../benchmarks/RELEASE-0.5.1.md) for measurements and limitations.
