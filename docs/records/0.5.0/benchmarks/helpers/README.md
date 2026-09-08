# Recorded benchmark instrumentation

The environment records identify these helpers by SHA256. These files retain
those exact bytes; `manifest.json` verifies them. Paths used while recording
were private staging paths, so invoke them from the corresponding source
checkout with your own prepared tool and image inputs.

- `guard-lima.py` attributes the selected Lima instance through its PID file
  and the Apple VM process's open instance disk.
- `guard-responsibility.py` additionally attributes Apple XPC helpers through
  a live macOS responsible PID and an explicit executable-path allow-list.
- `guard-exited-process.py` also records lookup results and distinguishes a
  snapshot process that has already exited from a live unidentified VM.
  Permission failures for a live process still invalidate the run.
- `run-original.sh` is the exact `benchmarks/run.sh` from recording source
  `1b0caabf34fbe359f11cd47cc87ce45dc8583682`.
- `run-competitor.sh` preserves the workload bodies, uses executable-only
  runtime PID selection, and gracefully stops Docker Desktop through its
  synchronous CLI rather than matching supervisor arguments with `pkill -f`.
  Its root lookup uses the current Git checkout because the file originally
  lived in private staging. Corrected OrbStack and Docker Desktop runs use it.

The responsibility API is a Darwin implementation detail used only by this
benchmark guard. The declaration also appears in this
[Chromium source](https://chromium.googlesource.com/chromium/src/+/24aa2a50ac03698e7ec5a11251a615b965f5874d/base/process/process_info_mac.cc).
Controlled restart diagnostics on the recording Mac confirmed the responsible
OrbStack Helper PID. An unavailable API does not authorize a live unknown VM.

The original protocol ignores setup and warm-up failure statuses. Measured-case
output is retained separately, including failed cases; untimed warm-up output
was discarded. Do not describe these records as independently verified warm
caches. The main release report explains case selection, incomplete attempts,
process-accounting corrections and the separately completed cold-start case.
