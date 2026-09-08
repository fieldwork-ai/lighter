# Preparation batch selection

A bounded M1 64/256/64 KiB comparison, three cold starts per arm, 8 vCPUs and
16 GiB guest RAM. Each boot has two verified 2 GiB Node allocations, 15 seconds
apart. Each arm also has an untimed start to prepare its app bundle. Stops are
followed by a two-second settling interval. Quiet guards and every repetition
are retained. Builds differ only in the preparation-batch constant, based on
57456cf; the exact 256 KiB patch and both executable hashes are recorded.

The first 2 GiB fill improves from about 689 to 644 ms (6.5%), comparing the
geometric mean of the two 64 KiB control medians with the 256 KiB median. CLI
startup is effectively unchanged at approximately 701 ms. Startup within-arm
CV is 3.6–5.6%; the first control allocation CV is 7.5%, while the middle and
return arms are about 0.3%. These small samples support the batch choice, not
a universal percentage claim. All 22 hardware tests pass at 256 KiB on M5.

Each batch still consists of independent 16 KiB owned objects. The change
reduces fault/mapping calls and does not combine allocation ownership or
weaken partial release. The default candidate selects 256 KiB; no additional
batch tuning is included in this qualification.
