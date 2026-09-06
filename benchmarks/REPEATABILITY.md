# Benchmark repeatability

A change between two published medians is an observation, not automatically a
speedup or regression. We measure variation between fresh runs of the same build
before interpreting a release comparison. This is an empirical baseline for the
specific host and workload below, not a universal tolerance such as “anything
under 5% is noise.”

## Protocol

The M1 has eight cores and 8 GB RAM. Each storage run uses a fresh guest with
eight vCPUs, 4096 MiB RAM and a 128 GiB sparse disk, the pinned package fixture,
and the same sequence: npm, pnpm, yarn, ripgrep, find, copy, remove. Package caches
are warmed before timing. Each case has three ordered repetitions; each fresh
run contributes its **median** to the between-run comparison. A new run starts
only after the monitored filesystem/indexing processes have been quiet for a
minute. Other container runtimes remain stopped. The M5 is shared and is not
used to establish this baseline.

Reproduce on the record host, with the release artifacts already built and the
tracked source clean:

```sh
bash scripts/records/record-variance.sh variance/m1/<version>-<session> 5
python3 benchmarks/variance.py benchmarks/results/variance/m1/<version>-<session>/run-*.csv
```

The runner refuses to overwrite an existing record and stops on a failed
workload or kernel stall. It saves the environment, source commit, artifact
hashes and raw CSVs. The report rejects mixed builds, hosts, artifacts, case sets
or incomplete repetitions. Failed attempts remain diagnostic evidence and are
not included in the baseline.

## How to read the spread

The generated table separates two sources of variation:

- **Between runs:** sample standard deviation and coefficient of variation
  (SD divided by mean) of the fresh-run medians. This is the relevant reference
  when comparing the medians published for releases.
- **Within a run:** the median CV of each run's three ordered repetitions.
  This includes cache and order effects. Repetitions are not independent fresh
  runs, and pooling all fifteen timings would confuse the two quantities.

The observed minimum and maximum are not confidence limits. Five runs establish
an initial observed spread; they cannot establish rare tail behaviour or rule
out drift across days, OS updates or other hosts. When a release appears to
change a workload, repeat both versions in alternating order with matching
settings. Preserve the original record even when a follow-up explains it.

## 0.4.0 versus 0.4.1 check

The first record comparison suggested that share `find` took 22.0% longer and
copying took 17.8% longer on 0.4.1. A focused matched check used fresh guests in
the order 0.4.0, 0.4.1, 0.4.1, 0.4.0, with the settings above, cache warming and
three repetitions each of find, copy and remove. Each version used its own VMM,
kernel and rootfs, with the same result-checking harness. The binaries were built
with Rust 1.98.0. These runs omit the preceding timed install sequence and must
not be pooled with the full storage baseline.

| Case | 0.4.0 run medians | 0.4.1 run medians | Change in median of run medians |
|---|---|---|---:|
| find | 120, 119 ms | 115, 116 ms | -3.3% |
| copy | 4417, 4259 ms | 4308, 4291 ms | -0.9% |
| remove | 2686, 2650 ms | 2533, 2593 ms | -3.9% |

Negative means less elapsed time. The large historical slowdowns did not
reproduce. Two runs per version do not establish a small speedup, either. In
both versions the first copy repetition took about 14 seconds and the next two
about 4.3 seconds, illustrating why repetition order matters.

[Raw matched-run CSVs and artifact stamps](results/variance/m1/0.4.0-vs-0.4.1/)
retain every successful observation, including the slow first repetitions.

## Same-build storage baseline

5 fresh runs of `099b91c` on `homeserver`. All timings are milliseconds. Each run contributes its median of three repetitions.

| Case | Run medians, in input order | Median | Between-run SD | Between-run CV | Observed range | Within-run CV, median |
|---|---|---:|---:|---:|---|---:|
| npm-install | 11797, 11660, 11273, 11194, 11166 | 11273 | 290.2 | 2.5% | 11166–11797 | 2.5% |
| pnpm-install | 6440, 6101, 5563, 5510, 5687 | 5687 | 398.5 | 6.8% | 5510–6440 | 10.3% |
| yarn-install | 11296, 11408, 11237, 10904, 11379 | 11296 | 202.2 | 1.8% | 10904–11408 | 7.4% |
| ripgrep | 183, 179, 212, 161, 209 | 183 | 21.5 | 11.4% | 161–212 | 160.6% |
| find-walk | 123, 120, 124, 120, 122 | 122 | 1.8 | 1.5% | 120–124 | 5.5% |
| copy-tree | 6105, 5659, 5480, 5796, 5853 | 5796 | 232.3 | 4.0% | 5480–6105 | 4.3% |
| rm-rf | 2529, 2545, 2506, 2551, 2529 | 2529 | 17.5 | 0.7% | 2506–2551 | 0.5% |

SD is sample standard deviation; CV is SD divided by the mean. Between-run CV describes the fresh-run medians. Within-run CV describes the three ordered repetitions and can include cache/order effects.

The observed range is descriptive, not a confidence interval or an automatic regression threshold. Same-build variation does not prove that a similarly sized cross-version change is noise. Use repeated, alternating version runs to check an apparent change.

Recorded on 6 September 2026, 19:12–19:43 UTC, on macOS 26.6.2 (25G83).
[Raw runs and source/artifact stamps](results/variance/m1/0.4.1/) and the
[host configuration](results/variance/m1/0.4.1/environment.txt) are retained.
These are the candidate at `099b91c`; the version label does not imply a
released or subsequently modified binary.

The medians drift down across the early npm and pnpm runs, so these five runs
are not evidence of stationary random noise. Ripgrep's 160.6% within-run CV
reflects a slow first read followed by warm reads; its between-run CV is 11.4%.
The observed fresh-run CVs range from 0.7% for removal to 11.4% for ripgrep.
Use the relevant case and protocol, rather than a single project-wide noise
percentage, when interpreting a change. This baseline covers share storage;
it establishes no variance threshold for networking, memory, boot or amd64.
