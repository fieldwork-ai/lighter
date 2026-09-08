# Demand-backed RAM prototype records

These are matched mode comparisons, not final signed release measurements.
Each directory includes raw repetitions, image/artifact fingerprints, the exact
source patch against dff2632d76d19d52dcffb0d8f4c2c79ef2005057, and outer quiet-host
guard logs. Source was dirty by design and frozen by the recorded patch; later
commit 57456cf includes the A3 implementation plus comment/documentation changes.

A1 uses a base-eager hotplug-demand prototype before whole-process failure
hardening. A3 includes failure hardening and full-base demand backing. Never
pool these binaries as if they were one identical run.

Reproduce each analysis from the repository root:

```sh
python3 docs/records/0.5.1/demand-prototype/analyze.py docs/records/0.5.1/demand-prototype/a1 --out /tmp/lighter-demand-analysis-a1
python3 docs/records/0.5.1/demand-prototype/analyze.py docs/records/0.5.1/demand-prototype/a3 --out /tmp/lighter-demand-analysis-a3
```

`quiet_host_enforced: false` in the inner recorder means it does not enforce
quiet itself. The outer guard does: its logs retain six consecutive samples
at most 5% aggregate host CPU before starting, and competing-VM checks during
the command. Failed/slow repetitions were not discarded. The analysis verifies
all these records and identical executable, payload, profile and image IDs.
