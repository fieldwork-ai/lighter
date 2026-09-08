# Shared-host copy diagnostic

The user requested five tree copies while using the M5. These are five
repetitions in one VM, with all three package-manager warm-ups and preceding
file-search reads. They are not five independent boots or a quiet release
benchmark. `qualified: false` is intentional; exit 0 only means the diagnostic
completed and cleanup checks passed.

All observations: 3695, 3892, 4008, 3837, 3289 ms. Median 3837 ms; sample CV
7.43% within this run. The 3289 ms best result is not a replacement for the
median and does not establish a typical speedup. These observations are
excluded from the selected release suite.
