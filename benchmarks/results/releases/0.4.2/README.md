# 0.4.2 M1 release records

The first complete suite is the primary performance record, selected before the
run. Suites 2 and 3 measure repeatability and continue the filesystem-daemon
soak. No daemon was restarted between suites. The controlled host is the M1;
the busy M5 supplied functional validation only.

- `baseline-0.4.1-*`: complete 0.4.1 suite after the user restarted fseventsd on
  September 7. Source `5f48cf03` contains the released 0.4.1 runtime.
- `published-0.4.1-*`: the previously published M1 record, retained as additional
  context without replacing the prespecified comparison baseline.
- `042-cc717e7-m1-{1,2,3}-*`: all raw timing samples, single memory/power windows
  and source/guest artifact stamps. Outliers are retained.
- `daemon.jsonl`: approximately five-second fseventsd CPU, physical-footprint
  and compressed-memory observations, plus benchmark-VM task footprints.
- `environment.txt` and `exit-code`: the recorded host environment and protocol
  completion status.
- `soak.sh` and `monitor-daemon.py`: the exact executed protocol. They assume
  the qualification checkout at `~/lighter` and helpers under `.logs/042`.
  Build the VM footprint helper with
  `clang -O2 benchmarks/task-footprint.c -o .logs/042/task-footprint`.

Regenerate the report with `python3 benchmarks/results/releases/0.4.2/report.py`.
Regenerate its figure with `plot.py` in the same directory, using the pinned
`plot-requirements.txt` environment. The figure's compressed-memory line is
already included in physical footprint; do not add the two values.

The daemon's original historical failure remains unreproduced. These records
measure the observed sessions and do not establish a root-cause fix or causal
performance changes between releases.
