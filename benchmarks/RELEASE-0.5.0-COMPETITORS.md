## Refreshed competitor records

All installed competitors use the same benchmark image archives and pinned
JavaScript tools as Lighter. Eight virtual CPUs and the host-specific 4/16 GiB
memory profile are verified from the running Docker engine. Guest kernels,
storage backends and available disk capacity remain product-specific and are
recorded rather than described as identical implementations.

- Colima 0.10.3 / Lima 2.1.3: an owned `lighter-050-bench` profile uses Apple VZ,
  VirtioFS, Rosetta and a 128 GiB sparse disk. The existing default profile is
  left stopped with its original settings.
- OrbStack 2.2.3 (20963): its existing engine is measured with Kubernetes and
  other Linux machines inactive. Original resource settings, image tags and
  running-container state are captured and restored. VM disk capacity is
  product-managed rather than forced to match Lighter's sparse disk ceiling.
- Docker Desktop 4.89.0 (238018): the selected configuration is Apple
  Virtualization.framework, VirtioFS and Rosetta. This comparison does not
  cover Docker VMM or other available backends. Settings and container state
  are captured and restored.

Lima's restarted hostagent is matched by its exact instance PID file. Reparented
Apple XPC VM processes are attributed through their exact open instance disk,
or through macOS's responsible PID and an explicit executable-path allow-list.
Ownership queries are recorded. A process that exits between the process snapshot
and ownership lookup is excluded only after a PID existence check proves it
has gone; a live unknown VM still invalidates the run. Docker Desktop's cold-stop harness
uses its synchronous `docker desktop stop --timeout 300` command and verifies
that application helpers exit; the old broad `pkill -f` could match the
supervisor's own arguments. The workload bodies remain identical. Every
competitor environment records the guard and harness hashes.

Docker's containerd image store can report an OCI manifest digest where its
classic image store reports a configuration digest. The derived identity map
verifies both from the original archive; it changes neither image content nor
workload inputs.

### Retained failures

M1 Colima's first host-share attempt completed every requested case except
pnpm (zero of three timed repetitions). A second attempt failed the same case
after one completed repetition and was stopped early. Repeated verbose
installs at the original shared path exposed `EMFILE` from `stat` inside the
pnpm store. Raising the container's open-file limit from 1024 to 65536 did not
resolve it. Host VM descriptor samples peaked near 9,200; the host's configured
per-process ceiling was 10,240. Those observations do not establish the exact
host-side fault. Colima's guest-disk pnpm case completed all three repetitions.
The table uses the first share attempt's complete non-pnpm cases and the
separately completed guest/amd64 stages, with an explicit unavailable pnpm
cell. The failed records and successful later stages retain their own labels
and exit statuses.

M5 OrbStack's first attempt stopped at cold start because a transient Apple
XPC helper had no known owner in the original guard. Controlled restarts
subsequently identified OrbStack Helper as its macOS responsible process.
The first attempt remains excluded; the corrected guard's rerun is retained
separately. Earlier M5 Colima attempts stopped before timing because Lima's
new argument form was unrecognized, then because Docker reported a manifest
digest instead of a configuration digest. Neither is a performance sample.

The next completed M5 OrbStack attempt exposed an accounting issue during
harness review: `pgrep -f OrbStack` also includes Python supervision processes
whose arguments contain allowed OrbStack executable paths. A live comparison
confirmed both extra PIDs. Its entire share stage is excluded from the tables
and repeated with executable-only PID selection. Guest and amd64 timing stages
remain usable. The corrected filter also applies to M1 OrbStack and both
Docker Desktop runs before measurement. Lighter uses its explicit VM PID.

The corrected M5 OrbStack share attempt completed every pre-boot case, but
its final boot observation stopped on another unknown transient XPC process.
Subsequent diagnostic restarts caught an XPC process disappearing between
snapshot and lookup. The original failed lookup was not logged, so this does
not prove that particular failure had the same cause. With the existence check
and query diagnostics added, a separate complete cold-start case passed. The
selected share record therefore combines the completed pre-boot cases and the
separately completed boot case. The incomplete attempt and its three boot
timings remain archived with exit status 1; those boot timings are excluded.
No timing was selected for being faster.

M5 Docker Desktop preparation first failed when our binfmt inspection tried
to read the write-only `register` control file. Skipping that control file fixed
the inspection before any timing began. This was a probe error, not a Docker
Desktop workload failure. The first timed M5 Docker Desktop suite passed all
three stages, and every restoration command succeeded. The M5's final quiet
observation retained the original fseventsd PID and ended at 12 MiB.

M1 Docker Desktop completed all three requested stages and all seven restoration
commands succeeded. A separate UDP verification also completed, with all seven
restoration commands succeeding. The case exclusions below apply to both hosts.

### Post-run setup audit

The retained measured-case output revealed shared-directory cleanup failures
on both Docker Desktop hosts: `rm -rf node_modules` reported “Directory not
empty” during npm, pnpm and Yarn setup. The original runner ignored setup
exit codes, so all timed installations still produced three values and the
supervisor returned success. That exit status alone is insufficient evidence
of valid inputs. Those install timings are excluded. Share read/copy/removal
cases depend on a later unlogged package-tree materialization after the same
failed setup sequence; they are conservatively excluded too, along with the
package-load memory readings whose successful-case output was discarded.
Independent share CPU, container start, polling visibility, network, idle power
and cold-start cases remain selected. Guest-disk and amd64 case diagnostics
contain no corresponding setup errors. Every original CSV and exit status is
preserved, with selection exclusions recorded separately.

The next harness rejects per-repetition setup errors/timeouts and failed
warm-up or input materialization, and retains successful memory-case output.
This does not retroactively establish discarded setup statuses for this release
record. No corresponding measured-case cleanup errors were found in the
Lighter, native or OrbStack outputs; M1 Colima's known pnpm failures remain
excluded as described above.

The network audit also found that an absent iperf summary could be mapped to
zero by the old parser. A separately guarded Docker Desktop UDP check on M5
retained all three complete raw JSON outputs: each has no iperf error and an
explicit receiver summary of zero bytes and zero packets, despite nonzero
sender counts. Thus zero was reproduced as an observed receiver result on
this path, not inferred from missing data. This is not a claim that all Docker
Desktop UDP networking fails. The corrected parser rejects missing/error data
and retains raw network output for future records. M1 independently reproduced the same explicit zero receiver result in all
three repetitions, with no iperf error. The raw JSON for both hosts is retained.

### M1 final-stage background activity

Docker Desktop's amd64 stage waited at the unchanged quiet gate while
WindowServer/loginwindow animation consumed CPU. Display sleep was attempted
before timing, then a two-second user-active assertion restored the display
when idle maintenance increased background load. The user-owned animated
wallpaper process and `duetexpertd` were temporarily paused after exact PID,
executable and ownership checks. Timing began only after the six consecutive
quiet samples passed. Those pauses apply to the final Docker Desktop amd64
stage, the separate UDP verification and the final quiet observation; they do
not apply to the earlier Lighter release sequences. Restoration is recorded
separately. Neither fseventsd process was stopped or restarted.

Both final quiet observations passed without restarting fseventsd: M1 ended at
5.59 MiB (0.0% median CPU, 0.2% maximum), M5 at 12 MiB (0.1% median, 0.9%
maximum). Each retained more than ten minutes, with zero collection errors or
invalid CPU readings. M1's paused wallpaper and user maintenance processes
were restored afterward, with exact identity checks and recorded timestamps.
