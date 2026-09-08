# M1 exact signed archive checks

All six stages passed using the final archive and bootstrap identified by
`environment.json`; inputs and daily configuration were unchanged afterward.
These are correctness checks, not performance measurements.

Cleanup logged Launch Services error -10814 for staged successor and kind
bundles. A subsequent registry/filesystem audit found no entries or remaining
test homes (`cleanup.json`). The temporary VMs stopped. These cleanup warnings
are retained; they do not change the successful runtime/upgrade assertions.

Private helper sources are retained in `../m5-signed-archive/`.
