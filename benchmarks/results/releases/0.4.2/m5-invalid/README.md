# Excluded M5 benchmark attempts

Both attempts used source `d0fbd114` and were stopped by the existing
other-VM guard. Neither is selected as a release performance record.
The M5 canonical CSVs remain the previously published 0.4.1 observations.

- Attempt 1 completed share and guest stages, then refused amd64 because the
  installed daily 0.4.1 VM was running. Its process start was observed at
  17:42:31 UTC, overlapping the end of guest timing at 17:42:34 UTC. The
  registered login job remained stopped; the origin of the CLI start was not
  established. The whole attempt is excluded.
- Attempt 2 added continuous detection of a daily-VM start. It completed the
  share stage, then refused guest because Colima/Lima was running. The Lima
  processes started at 17:56:51 UTC, during the share stage, so the whole
  attempt is excluded. A daily-VM-specific guard does not detect every
  competing hypervisor during a stage; the existing next-stage guard caught
  this one.

There was no completed post-suite observation on either attempt. The monitor
stopped when its parent runner exited; a stale final sample does not indicate
a daemon hang. Raw timings and daemon/background-process observations are
retained here as diagnostics, not valid comparative performance evidence.

The original daily configuration (16 GiB), executable wrapper and login plist
were unchanged, and the daily VM was restored. No competing Colima VM was
stopped. A clean M5 repetition requires coordination with the other VM work.
