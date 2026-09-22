# Memory for a guest that is never idle: 2026-09-21

Status: built and measured for 0.7.3 (results at the end); the design as proposed, with one correction about what the cache was. Written after an evening on the
M5 (48 GB, 0.7.2, the daily stack: Frigate on the Neural Engine, Home
Assistant, Whisper, Piper, Mosquitto, and the app's dev stack of Postgres,
MinIO, Mailpit, a query runner and seven s3-brokers) that began as "lighter
is pulsing and sitting at 12 GiB while I am not using it". This note is what
was found, what the field does about the same problem, and the design that
follows from both. It also carries the Neural Engine helper's idle CPU,
which is the same complaint from the other side: a machine that is warm all
day must cost nothing when it has nothing to do.

## The premise

Every mechanism in lighter that gives cache back to the Mac waits for a
state the machine that matters never reaches. `docs/guest-memory-2026-09-20`
already said it of the virtio-mem range: "this machine always has
containers", and the range went. The same sentence applies to three more
gates.

| mechanism | gate | on a warm machine |
|---|---|---|
| free page reporting | none | works, for free runs of 32 KiB and up |
| trims (`memory.reclaim` to a resting level) | containers' cgroup unpopulated | never |
| balloon offer of spare memory | whole guest under a tenth of a core for 3 s | never: Frigate alone is a third of a core |
| hourly idle pass (DAMON `pageout`, `young` filter) | none | runs; takes pages one to two hours untouched |
| host pressure ramp, reclaim before each step | Mac compressing, at Warn, or overcommitted | works; capped at an eighth of RAM while only compressing, a quarter at Warn, a half at Critical |

So on the daily driver the steady state is set by two numbers, an hour and
a quarter, and neither was chosen for this case. The gates themselves are
right for what they guard: each records a measured regression (the seesaw
on copy-tree, the cold ripgrep, the throttled stack under `memory.high`).
The design below leaves every one of them where it is and adds the piece
that is missing, rather than loosening a gate that was closed for a reason.

## What was measured

With Frigate recording: VMM footprint 12.5 GiB (16 GiB guest less a balloon
held at 4096 MiB), guest page cache 8.8 GB, containers' anonymous memory
about 1.3 GB, guest CPU a third of a core. The Mac was at Warn
(`kern.memorystatus_vm_pressure_level` 2) with 7.9 of 9.2 GB of swap in use. Frigate wrote 1.2 to 1.4 GiB of
recordings an hour (`~/ha/media/recordings`, 19:00 and 20:00 on the 21st),
every page written once and never read, and the first draft of this note
blamed them for the cache. They were not it: the recordings go to the Mac
through the share, and a file written through virtiofs is not held in the
guest's page cache at all (a 300 MB write through the share charged the
writer's cgroup 1 MiB; reading it back charged 301). An hour's sampling
with Frigate recording had its cgroup's cache flat at 300 to 550 MiB. What
the guest was holding was the engine's: image layers extracted and builds
run that day (4.2 GB in the engine's cgroup after one image build), plus
what the dev stack's containers had read and written on the guest's own
disk. The loop below tends both cgroups; what it cannot reach, and need
not, is the share.

With Frigate stopped and the rest of the stack up, the guest went quiet, the
offer went out, and within minutes the balloon target stood near 10 GiB,
the footprint at 5.2 GB and the cache at 0.8 GB. The policy frees
everything it can. It needs the guest to stop first, and a daily driver
does not stop.

The agent's line to the host is fresh: `/proc/meminfo` and PSI, 32 bytes
over vsock, four times a second while anything happens and once a second
after ten idle seconds. The virtio statistics queue is not offered and is
not needed.

Two things this was not. The system-wide CPU pulse that started the
evening was Claude Code's status line (`refreshInterval: 10` across some 35
open sessions, about 1,250 process launches in two seconds, every ten
seconds), found with `fs_usage -f exec` and nothing to do with lighter. And
the 543 balloon target changes in 24 hours are mostly the offer tracking
free memory a few hundred MiB at a time; whether any release and re-ramp
cycles survive on 0.7.2 has not been separated by version and is not
claimed here.

## What the field does

The always-warm guest is the datacenter's ordinary case, and the two
published systems that run it at scale agree on the shape and disagree on
the signal.

**Google, software-defined far memory (ASPLOS 2019).** `kstaled` walks
accessed bits every 120 s and keeps an age per page; `kreclaimd` evicts
pages older than a cold-age threshold. The threshold is not a constant. It
is the lowest age, per job, that keeps the promotion rate (evicted pages
touched again) under a target share of the job's working set per minute,
read from a promotion histogram the kernel maintains. Age decides *which*
pages; measured re-use decides *how far*.

**Meta, Transparent Memory Offloading (ASPLOS 2022).** A userspace agent,
Senpai, asks the kernel to reclaim a sliver from each cgroup every six
seconds and lets the kernel's LRU choose the pages:

    reclaim = current_mem * reclaim_ratio * max(0, 1 - psi_some / psi_threshold)

with `reclaim_ratio` 0.0005, `psi_threshold` 0.1% and a ceiling of 1% of
the workload per period, one configuration for the whole fleet. Six
seconds is "enough time to measure the delayed impact (refaults) of
reclaimed memory". The harm signal is PSI, because the kernel's refault
detection feeds it: a stall on a page that was recently evicted counts, a
first read of a file does not. Contraction takes minutes; "adaptation to
workload expansion is immediate".

Two details of TMO are lighter's own history told by someone else. Senpai
first drove reclaim by lowering the cgroup's memory limit, and it went
badly: a crashed agent left the limit behind, a growing workload blocked
against it, "pressure spikes significantly above workload tolerances". That
is the `lighter.cachebound` incident (36 tasks in D state, 782,575 throttle
events). Meta's answer was to add the stateless `memory.reclaim` file to
the kernel, which is what lighter's trims and pressure ramp already use.
And the cures that failed here ("a reclaim request whenever the compressor
moved doubled the install", "anything that takes a gigabyte of it at once
is paid on every page") failed on step size and on having no feedback, not
on the mechanism: Senpai's step on a cgroup this size is a few megabytes,
two orders of magnitude smaller, and it stops itself.

**WSL2** (`autoMemoryReclaim=gradual`) waits for five minutes of low CPU
and then reclaims a fixed slice a minute through `memory.reclaim`. It is
the idle gate again, and it fails the same way on a machine that is never
idle. crosvm, Cloud Hypervisor and Firecracker offer a balloon and free
page reporting and leave policy to the operator. On pacing both ramps, on
treating host compression as a signal and on host-page balloon units,
lighter is already past what these ship.

The practice, then: **continuous, small, proactive reclaim through the
guest kernel's own LRU, with the amount set by a closed loop on measured
harm.** Fixed ages and fixed fractions are what one writes before one has
the feedback signal. lighter has the signal (PSI is already on the line to
the host) and does not yet close the loop.

## Design

### Two paths, by timescale

The quarter-of-RAM cap raised the question "why not take everything?", and
the answer is that the ramp acts in seconds and cannot wait to see whether
it hurt. A path with no feedback needs a bound; a path with feedback does
not. So:

- **Acute** (seconds): the host pressure ramp, as it stands. Reclaim the
  step from the containers' cgroup, inflate by the step, capped at a
  quarter and a half, paced both ways. Unchanged.
- **Chronic** (minutes): a new loop in the agent that keeps the containers'
  cache near their working set all day, so the acute path has little left
  to do when the Mac tightens. Bounded by harm, not by a fraction.

### The chronic loop

In the agent, on its own thread (a `memory.reclaim` write is synchronous
and must never hold the tick), every six seconds, for the containers'
cgroup and the engine's:

    step = current * ratio * gain * max(0, 1 - some / threshold)
    step = min(step, current / 100)
    write "{step} swappiness=0" to memory.reclaim

- `threshold` 0.1%, `ratio` 0.0005, the period six seconds: Meta's
  production values, taken as the starting point because they were tuned
  across more workloads than lighter will see, and because TMO reports the
  result "not very sensitive" to them.
- `some` cannot be what Senpai reads. Senpai reads each cgroup's own
  `memory.pressure`, and lighter boots with `cgroup_disable=pressure`
  (`crates/lighter-cli/src/run.rs`): per-cgroup accounting "cost a context
  switch and a wakeup every two seconds on an idle machine", and that was
  paid for once already. The guest has `/proc/pressure/memory` only. That
  matters more than it looks, because a `memory.reclaim` write is itself a
  memory stall charged to the task that makes it. In TMO the charge lands
  in Senpai's cgroup and the workload's figure stays clean; system-wide,
  the loop would read its own work as harm and throttle on it (the agent
  already met this: a `need` on `some` "handed the balloon back the moment
  the host had asked for it", m6b). So the loop takes the delta of the
  `some` line's cumulative `total` microseconds across its six seconds, not
  `avg10` (exact, and without ten seconds of lag), and subtracts the wall
  time of its own reclaim writes in that period:

      some = max(0, d_total - own_write_usec) / period_usec

  Subtracting the whole write discounts any stall that overlapped it, so
  the figure errs toward reclaiming; the writes are milliseconds in six
  seconds and the error is bounded by them. The host's acute reclaim
  requests are timed and subtracted the same way.
- `swappiness=0`, file cache only. A workload's heap is never swapped
  behind its back (the trims' rule), and the Mac's compressor already does
  for guest anonymous memory what zswap does in TMO; doing it twice buys
  nothing.
- `gain` is the host's word. It is the one new field on the control
  channel: 1 at Normal, higher while the Mac compresses, is at Warn or is
  overcommitted by the existing compressor and swap tests, highest at
  Critical. Host need says how hard to push; guest harm says when to stop.
  A wrong gain costs convergence time, never a stalled container, which is
  the property the tier table proposed earlier in the evening lacked.
- Below a floor (the cgroup under a 64th of RAM, the trims' resting level)
  the loop does nothing.
- One loop over the containers' parent (`/sys/fs/cgroup/docker`) and one
  over the engine's, not one per container. With a system-wide signal
  there is nothing to attribute a stall to, so a finer loop would be
  precision the input does not have. The consequence is accepted and made
  visible: a container thrashing against its own `memory.max` holds `some`
  over the threshold and pauses the loop for everyone, which is the
  conservative failure, and the counters say so. The engine's cache is
  image layers that running containers map without owning the charge; the
  LRU keeps mapped pages longest and a slow container start would show as
  stall, but it is the cgroup to watch in the first week's counters.
- On the M5 tonight the two cgroups held 1697 and 430 MiB of the guest's
  2228 MiB of file cache, so the cache is where the loop can reach it.
  Files written through the share are not guest cache and are not the
  loop's concern (above).

What the gain means for a writer at 1.3 GiB an hour onto the guest's disk
whose pages are never read, a build cache or a log shipper (equilibrium
where drain equals inflow; and the time to halve a cache nothing refills,
which is what the storage benchmark's cases leave behind):

| gain | drain | dead cache held | half-life of a cache nothing refills |
|---|---|---|---|
| 1 | 0.05% per 6 s | about 4.4 GiB | 2.3 hours |
| 8 | 0.4% per 6 s | about 0.55 GiB | 17 minutes |
| 20 (the 1% ceiling) | 1% per 6 s | about 0.22 GiB | 7 minutes |

Contraction takes minutes, as TMO says of its own; the acute path is for
seconds. And a five-minute benchmark case cannot show an equilibrium: it
can show the drain (the loop's `reclaimed` counter), the harm (the
re-readers' times) and the cost (host CPU per guest CPU), which is what the
warm case reads.

At gain 1 the hourly idle pass is the tighter of the two for write-once
data, which is the honest reason both exist in 0.7.3: the pass bounds what
a quiet Mac carries, the loop bounds what a short one carries. Whether the
pass can retire once the loop is proven is a question for 0.7.4 with data,
not one to settle now.

Failure modes, each by construction rather than by care: the agent dying
leaves nothing behind (`memory.reclaim` is stateless); a build's working
set refaults, PSI rises past the threshold and the step goes to zero within
one period; `-EAGAIN` from a cgroup with nothing reclaimable is the
expected answer and is not retried.

### Getting the freed pages to the Mac

Reclaimed cache comes back in file-sized pieces. Reporting returns runs of
32 KiB and up; the rest needs a compaction pass or the balloon, which takes
a host page from any free list.

For 0.7.3, compaction, and only when the Mac wants the memory: once the
loop has reclaimed 256 MiB since the last pass *and the gain is above 1*,
one `compact_until_reportable`, as the idle pass does after an eviction
(on the M1 that had 2227 of 2258 MiB back with the host in fifteen
seconds). At gain 1 nothing on the Mac is waiting for the pages, reporting
returns what it can, and the fragments are free memory the next allocation
uses; no pass is spent on them. Under pressure the ramp's balloon is
inflating as well and takes fragments as they are. A pass costs the next
command, and one every few minutes on a working machine is not something
lighter has run before: it is the least certain piece of this release, and
the warm case reads the re-reader's wall time for exactly this reason.

Held for 0.7.4, the balloon's offer. On a warm machine it never goes out,
because its gate is guest CPU, and CPU was a proxy: the seesaw that gate
prevents came from work *allocating* into a balloon that had just inflated,
and a camera stack burns steady CPU while allocating almost nothing. The
gate that says what is meant, for guests of 8 GiB and up: no memory stall,
free memory above the reserve and a quarter for three seconds (as now), and
free memory not falling across those three seconds, with the release rule,
the reserve and the host's paced ramps unchanged. It has no precedent and
it touches the path that seesawed, so it does not ship beside the loop,
where a regression would have two suspects.

### What is deliberately not changed

Trims gated on an empty cgroup. Offering free memory, never
`MemAvailable`. No default cache bound. The ramp's caps, steps, patience
and pressure memory. The idle pass and its horizon. Each has a measurement
behind it and none is the cause here.

## Where a laptop is not Meta's fleet

The architecture is borrowed with confidence; the numbers are borrowed as a
start. Three differences decide whether they hold.

- **Few tasks.** PSI over a fleet cgroup with hundreds of threads is a
  smooth figure. Over a guest with a handful of busy tasks it is lumpy,
  and 0.1% of six seconds is six milliseconds: one slow fault. The
  threshold may need to be read over a longer window than the period it
  gates. The counters will say.
- **Refaults do not cost the same.** A page evicted from the guest's own
  disk is back in about a hundred microseconds. A page from the macOS
  share comes back through the file server and costs many times that. PSI
  measures time lost, not pages, which is why it is the control signal and
  the refault count is only logged; but it makes share-heavy work (a
  `node_modules` tree on the share, `git status` over it) the workload
  most likely to be hurt, and the warm case must contain it.
- **Latency over throughput.** A fleet tolerates a build two percent
  slower. A person notices `git status` going cold. If the re-reader
  moves, the ratio comes down, whatever Meta runs.

## The Neural Engine helper

`lighter ane-host` sat at 43 to 53% of a core with Frigate sending about
five frames a second. A three second `sample`: 1,449 on-CPU samples in
`onnxruntime::concurrency::ThreadPoolTempl::WorkerLoop`, its spin lambda
and `SpinPause`; 16 in CoreML's prediction; about 10 copying to and from
the Neural Engine. ONNX Runtime's intra-op workers spin after each task by
default so the next starts sooner, and a client that sends a frame every
100 to 200 ms never lets them sleep. CoreML does the compute, so the pool
is waiting for work that does not exist. With Frigate stopped the helper
reads 0.0%.

The session `create` function in `crates/lighter-vmm/src/ane/ort.rs` builds its
session options and appends the CoreML provider; it sets no threading options, so
every default applies. The fix, before `CreateSessionFromArray`:

- `AddSessionConfigEntry("session.intra_op.allow_spinning", "0")` and
  `"session.inter_op.allow_spinning"`, for every session.
- `SetIntraOpNumThreads(1)` only when the provider is CoreML. A
  `Units::Cpu` session does its real work on that pool and keeps the
  default.

Cost: a futex wake per inference, tens of microseconds against 3.5 to 6 ms.
Verified the way it was found: `sample <pid> 3` and `top` before and after
with Frigate attached, and the detector's per-frame time unchanged.

The general rule this is an instance of, worth a gate so it is not found by
a user a third time (the first was the guest's 38 wakeups a second): every
host process lighter starts reads under 1% of a core between requests with
a client attached. One case in the suite: start the stack's shape (an ANE
client at 5 Hz, a GPU client idle), wait a minute, read `top`.

## The VMM's pulse: named, and not a defect

With every container idle the VMM read 35 to 50% of a core for a second
every two to five seconds and 3% between. It is the stack's health checks.
`docker events --filter event=exec_start` over 22 seconds:

    t=0   minio, postgres, mailpit          (5 s interval)
    t=2   s3-broker, datalake-writer, query-runner   (10 s, `node -e fetch(...)`)
    t=5   minio, postgres, mailpit
    t=10  minio, postgres, mailpit
    t=12  s3-broker, datalake-writer, query-runner
    t=15, t=20 ...

Gaps of 2, 3 and 5 seconds, which is the rhythm in Activity Monitor. No
long-lived guest process used two ticks in any second; the work is all in
processes that live for a fraction of one. Each check is a `runc exec`
into the container's namespaces, and three of them start a Node runtime to
make one HTTP request. The guest's `/proc/stat` showed 35 busy ticks on a
heavy second (0.35 core-seconds) against about 0.5 on the host: the VMM
costs roughly 1.4 times what the guest spends, which is virtualisation
doing its job, not a poll window burning a core.

So there is nothing to fix in lighter here. Two things follow. The ratio
of host CPU to guest CPU on a bursty idle stack is worth recording in the
warm case below, because 1.4 is the number a regression would move. And
the cheap win is in the stack, not the runtime: a dev stack does not need
a five second liveness probe, and a `node -e` that boots a runtime to call
`/health` costs a hundred times what `curl` or a static probe does.

## How it is proven

No case in the suite is a warm stack, which is how this went unmeasured.
One new case, time-compressed so it is minutes and not hours (the smallest
reproducing sequence first, a full record only at the release candidate):

- a writer producing write-once file data at a fixed rate at a steady
  third of a core, an idle Postgres, and health checks at the daily
  stack's cadence;
- beside it, two re-readers that walk the same tree every few minutes,
  one on the guest's own disk and one on the macOS share: the workloads
  the loop must *not* hurt, the share being the dearer refault;
- read at Normal and under induced host pressure: VMM footprint, the
  cgroups' `workingset_refault_file` and the guest's `/proc/pressure/memory` totals, the
  re-readers' wall times, and host CPU over guest CPU for the run (1.4 on
  the M5 today).

Accepted when, under host pressure, the writer's dead cache settles near
the table above, the re-reader's time is within day-to-day noise of the
loop being off, and the guest's `some`, net of the loop's own writes,
holds at or under 0.1%. Then the existing suite once, A against B, on the 16 GiB guest
and on the M1's 4 GiB, for the cases that caught every earlier attempt:
ripgrep repetitions, copy-tree, the pnpm and npm installs, the minute-after
footprint.

The loop logs one line when it changes state and keeps counters the CLI
can show (`reclaimed`, `refaults`, `some`, the current gain). A policy
nobody can see is a policy nobody can trust, and the next report of "12
GiB while idle" should be answerable from `lighter doctor` in one read.

## Decisions, with a recommendation for each

The rules applied, none of them new: prefer a constant someone measured at
scale to one guessed here; change one variable a release so a regression
has one suspect; put feedback before tuning; ship the instrument with the
mechanism; and leave alone what has a measurement behind it.

1. **The loop's constants: Meta's, unchanged** (`ratio` 0.0005,
   `threshold` 0.1%, six seconds, 1% ceiling). They are the only values
   with a fleet behind them, TMO reports the outcome insensitive to them,
   and a gentler first guess would replace a measured number with an
   unmeasured one for the sake of feeling careful. The feedback is the
   safety, not the ratio.
2. **The harm signal: system-wide `some`, by `total` deltas, net of the
   loop's own writes. `cgroup_disable=pressure` stays.** Turning per-cgroup
   PSI back on would be Senpai to the letter and would undo an idle-wakeup
   fix already paid for, to buy attribution the design does not need. If
   the first week's counters show the net figure too noisy to control on,
   that is the moment to price the wakeup against it, with numbers.
3. **The gain: 1, 8, 20, selected by the host's existing tests** (Normal;
   compressing, Warn or overcommitted; Critical). No new host signal and no
   new thresholds: those tests already carry the hysteresis that 09-16 and
   09-20 paid for. The gain is feed-forward under a feedback limit, so
   being wrong by a factor of two costs minutes of convergence and nothing
   else. Log it; revisit with a week of the M5's counters.
4. **The offer's new gate: not in 0.7.3.** It is the one piece with no
   precedent, it touches the path that seesawed, and shipping it beside the
   loop gives any regression two suspects. In its place the loop reuses
   what the idle pass already does, a compaction per 256 MiB reclaimed, and
   only while the gain is above 1. If the warm case shows the footprint
   not following the cache down, the gate is 0.7.4's one variable, with
   the data to judge it.
5. **The pulse: out of scope, closed.** It is workload. What stays is the
   host-to-guest CPU ratio as a recorded number in the warm case.
6. **Where: develop on the cloud box, A/B on the M1's 4 GiB guest, the M5
   once for the warm-stack check** at a time of Nick's choosing, because it
   is the only machine with the real stack and it is his daily driver.

So 0.7.3 is three things, each measurable alone: the chronic loop with its
counters, the helper's spinning fix with the idle-cost gate, and the warm
case in the suite. Order: the warm case first, so the loop is judged
against a number that existed before it; then the spinning fix, which is
independent and small; then the loop.

Checked before any of it is built, because the design rests on them: that
cache written through the share is charged to the writer's cgroup; what a
`memory.reclaim` write of a few megabytes costs in wall time on this guest,
clean and dirty; and how lumpy the net `some` figure is on the M5's stack
over an hour with the loop off.

## Sources

- Weiner et al., "TMO: Transparent Memory Offloading in Datacenters",
  ASPLOS 2022, sections 3.2 to 3.4; and Meta's engineering post of
  2022-06-20 for the formula as written.
- Lagar-Cavilla et al., "Software-Defined Far Memory in Warehouse-Scale
  Computers", ASPLOS 2019, sections 4.2, 4.3 and 5.1.
- Microsoft, "Windows Subsystem for Linux September 2023 update"
  (`autoMemoryReclaim`), and "Memory Reclaim in the Windows Subsystem for
  Linux 2" (2019).
- Linux `Documentation/admin-guide/cgroup-v2.rst` (`memory.reclaim`,
  `memory.pressure`, `memory.stat`), and
  `Documentation/admin-guide/mm/damon/usage.rst` (online `commit`).
- ONNX Runtime, `onnxruntime_session_options_config_keys.h`
  (`session.intra_op.allow_spinning`, `session.inter_op.allow_spinning`).

## Results, 2026-09-22

Built as proposed: the loop (`guest/agent/src/warm.rs`), the gain on the
control channel, the doctor row, the `warm` case, the helper's spinning
fix and its gate. Two things the building found that the proposal did not.

**The host's word had never reached the guest.** `ask_agent` wrote its line
into the host end of the socket pair it handed to `vsock.open`, which
nothing forwards, and read the pair for an answer the device had queued on
the connection instead. Every reclaim asked ahead of a balloon step since
0.7.2 timed out after thirty seconds at debug level, and no machine ever
logged one that worked. Found when the gain went the same way and the
agent's report said `gain=1`. Both directions now go through the device,
and the warm case fails if the gain the host said is not the gain the
agent reports. The reclaim-ahead is therefore new in 0.7.3 and was run as
its own arm.

**The recordings were not the cache.** Files written through the share are
not held in the guest's page cache (a 300 MB write charged 1 MiB; the read
back charged 301), and an hour's sampling with Frigate recording had its
cgroup flat at 300 to 550 MiB. The guest's cache was the engine's image
builds and the dev stack's own writes, which the loop reaches.

The warm case, one gigabyte written to the guest's disk in the first two
minutes and held for three more, the two re-readers every 20 s, 300 s,
one run per arm (`benchmarks/results/machines/*-warm-0.7.3/`):

| machine, guest | arm | footprint MiB | cache MiB | re-read disk / share ms | reclaimed MiB | held (periods) |
|---|---|---|---|---|---|---|
| M5, 16 GiB | loop off | 4524 | 2999 | 289 / 521 | 0 | |
| M5, 16 GiB | gain 8 | 4137 | 2514 | 292 / 541 | 582 | 0 of 58 |
| M5, 16 GiB | gain 20 | 4030 | 2043 | 315 / 561 | 1255 | 2 of 58 |
| M1, 4 GiB | loop off | 3023 | 2070 | 1047 / 2003 | 0 | |
| M1, 4 GiB | gain 8 | 2964 | 2003 | 1086 / 1991 | 372 | 10 of 62 |
| M1, 4 GiB | gain 20 | 3093 | 1921 | 1236 / 2212 | 778 | 18 of 62 |

On the 16 GiB guest the drain is what the table above predicted (gain 20:
a third of the cache in three minutes, a half-life near seven), the
re-readers move under 10%, and the loop was held by the guest's own stall
twice in five minutes. On the M1's 4 GiB guest the cache is bounded by the
guest, not by the policy, the loop is held a third of the time by the
writer's own reclaim pressure (correctly), and the footprint barely moves
because the Mac's 8 GB, not the guest's cache, decides it; the re-readers
at gain 20 read 18% and 10% slower than off, inside what that machine's
single runs show run to run (the first chain's share re-reads ranged 6.5
to 9.0 s across four arms at the old writer shape), but not something to
claim as nothing. The 4 GiB guest is where the constants would be
tightened first if a second run agrees.

The storage and memory suite, three reps, the loop off (0.7.2 as it ran)
against on at the gain each Mac's own pressure gave it (both were
overcommitted: gain 8):

| case | M5 off | M5 on | M1 off | M1 on |
|---|---|---|---|---|
| npm-install s | 7.3 / 7.2 / 6.5 | 6.7 / 6.6 / 6.5 | 16.9 / 14.8 / 13.1 | 12.8 / 12.5 / 12.4 |
| pnpm-install s | 4.8 / 4.0 / 3.9 | 5.6 / 3.9 / 3.9 | 8.3 / 6.5 / 7.2 | 7.7 / 6.3 / 6.8 |
| yarn-install s | 5.8 / 5.8 / 5.6 | 5.4 / 5.1 / 5.7 | 15.7 / 15.9 / 12.9 | 15.7 / 15.1 / 11.8 |
| ripgrep ms | 1715 / 85 / 80 | 1711 / 87 / 81 | 6439 / 363 / 143 | 6309 / 203 / 132 |
| copy-tree s | 4.8 / 3.5 / 4.2 | 4.8 / 3.3 / 4.3 | 8.4 / 11.1 / 8.4 | 9.1 / 11.6 / 7.8 |
| rm-rf s | 3.6 / 2.4 / 2.6 | 2.6 / 3.4 / 2.5 | 3.1 / 2.8 / 2.8 | 2.9 / 3.0 / 2.9 |
| memory peak / 15 s / 60 s MiB | 8804 / 1583 / 1543 | 8502 / 1392 / 1341 | 3275 / 3216 / 916 | 3164 / 916 / 847 |

Nothing slower outside run-to-run noise on either machine; the minute-after
footprint 200 MiB lower on the M5 and the M1's fifteen-second reading a
third of what it was. The reclaim-ahead arm alone (M1, `s-ahead`) read
3439 / 1235 / 1092.

The helper: the m10 gate's client at 5 Hz reads the VMM at 2.6% of a core
with spinning off against 31.1% with `LIGHTER_ANE_SPIN=1`, inference 0.80
ms against 1.76. The m6 gate's idle-pass boot passes with the ramp off;
its ease check still fails on the M5 tonight because the Mac is
overcommitted (7.6 GB in swap) and is steered as at Warn whatever the test
file says, which is the rule 0.7.2 added and not something 0.7.3 changed.

Decisions 1 to 6 as recommended. The offer's new gate stays held for
0.7.4, with the M1's gain-20 re-reads as the first thing to re-measure.

