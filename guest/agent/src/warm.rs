//! Keeping the containers' cache near their working set while they run.
//!
//! Everything else here that gives cache back waits for a state a daily
//! driver never reaches: the trims for an empty container hierarchy, the
//! balloon's offer for a quiet guest, the idle pass for an hour without a
//! touch. A machine whose containers are always up sat at 12.5 GiB with 1.3
//! GiB of container memory, on a Mac that was swapping, until its owner
//! stopped a container (the M5, 2026-09-21; `docs/warm-guest-memory`).
//!
//! The loop is Meta's Senpai (Weiner et al., "TMO: Transparent Memory
//! Offloading in Datacenters", ASPLOS 2022): every few seconds ask the
//! kernel to reclaim a sliver of a cgroup through `memory.reclaim`, let its
//! LRU choose the pages, and let measured stall decide how large the sliver
//! is:
//!
//! ```text
//! step = file * RATIO * gain * max(0, 1 - some / THRESHOLD),  at most file / 100
//! ```
//!
//! The cures that failed here took a gigabyte at once with nothing to stop
//! them ("a reclaim request whenever the compressor moved doubled the
//! install"). A step here is megabytes, and a working set that starts
//! refaulting raises `some` and brings the step to nothing within a period.
//! `memory.reclaim` is stateless, so an agent that dies leaves no limit
//! behind: what `memory.high` did to a daily stack (`lighter.cachebound`)
//! is what Senpai's first version did to Meta's, and is why the file exists.
//!
//! Three things differ from the paper, each for a reason found here.
//!
//! The base is the cgroup's file LRU, not `memory.current`. Reclaim is file
//! only (`swappiness=0`: a workload's heap is never swapped behind its back,
//! the trims' rule, and the Mac's compressor already does for guest heap what
//! zswap does in TMO), so a cgroup that is mostly heap would otherwise have
//! its small cache drained at the rate its heap earned.
//!
//! The stall figure is the whole guest's, net of the loop's own writes.
//! Senpai reads each cgroup's `memory.pressure`; lighter boots with
//! `cgroup_disable=pressure`, because that accounting cost an idle guest a
//! wakeup every two seconds. A reclaim is a memory stall charged to the task
//! that asks for it, so system-wide the loop would read its own work as harm
//! (the host's reclaim requests did exactly that to `need`, m6b). The delta
//! of the `some` line's cumulative `total` across the period, less the wall
//! time of the reclaim writes made in it, is the rest of the guest's stall.
//! Measured on the M5: a 4 MiB reclaim of clean cache charges nothing
//! readable and 96 MiB charged under a millisecond, so the correction is
//! small; it is kept because dirty cache and a slow disk make it large.
//!
//! The gain is the host's word (`gain <n>` on the control channel): 1 on a
//! Mac with memory to spare, more while it compresses, swaps or reports
//! pressure. Host need says how hard to push and guest stall says when to
//! stop, so a gain that is wrong costs minutes of convergence and never a
//! stalled container. A gain not refreshed for three minutes is 1 again: a
//! host that stopped talking is not a host under pressure.
//!
//! No PSI, no loop: `psi=0` on the command line leaves nothing to stop it.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Long enough "to measure the delayed impact (refaults) of reclaimed
/// memory" (TMO, 3.3).
pub const PERIOD: Duration = Duration::from_secs(6);
/// 0.0005 of the cgroup a period at gain 1, and nothing at or above 0.1% of
/// the period stalled: Meta's production values for every workload it runs.
const RATIO_PPM: u64 = 500;
const THRESHOLD_PPM: u64 = 1000;
/// Never more than a hundredth of the cache in a period, whatever the gain.
const CEILING_DIV: u64 = 100;
/// The gain that reaches the ceiling; more would change nothing.
pub const GAIN_MAX: u32 = (1_000_000 / CEILING_DIV / RATIO_PPM) as u32;
const GAIN_TTL: Duration = Duration::from_secs(180);
/// A step under this is not worth a trip through reclaim.
const MIN_STEP: u64 = 256 << 10;
/// Reclaimed cache is free in file-sized pieces, and reporting returns runs
/// of 32 KiB and up. One compaction pass per this much reclaimed, and only
/// while the host wants the memory: at gain 1 nothing on the Mac is waiting
/// for the pages and the fragments are the next allocation's.
const COMPACT_EVERY: u64 = 256 << 20;
/// One line an hour when there is something to say, as the idle pass has it.
/// A line each time the loop was held or let go was written first: a guest
/// with a handful of busy tasks crosses a threshold of six milliseconds in
/// six seconds many times an hour, and the machine's log is read by people.
const REPORT_EVERY: u64 = 600;

/// What to ask of a cgroup whose file LRU holds `file` bytes and rests at
/// `floor`, at `gain`, with `some_ppm` of the last period stalled.
pub fn step(file: u64, floor: u64, gain: u32, some_ppm: u64) -> u64 {
    if file <= floor || some_ppm >= THRESHOLD_PPM {
        return 0;
    }
    let headroom = THRESHOLD_PPM - some_ppm;
    let gain = u64::from(gain.clamp(1, GAIN_MAX));
    // Multiply before dividing: a 2 GiB cache at gain 20 is 2e9 * 500 * 20 *
    // 1000 = 2e16, inside a u64; u128 for the guest nobody has built yet.
    let wanted = (u128::from(file) * u128::from(RATIO_PPM * gain * headroom)
        / u128::from(1_000_000 * THRESHOLD_PPM)) as u64;
    let step = wanted.min(file / CEILING_DIV).min(file - floor);
    if step < MIN_STEP { 0 } else { step & !4095 }
}

/// The share of a period some task spent stalled on memory, in millionths,
/// not counting `own_usec` of reclaim this agent asked for itself.
pub fn some_ppm(total_delta_usec: u64, own_usec: u64, period_usec: u64) -> u64 {
    if period_usec == 0 {
        return THRESHOLD_PPM;
    }
    total_delta_usec.saturating_sub(own_usec).saturating_mul(1_000_000) / period_usec
}

/// `total=` of the `some` line of `/proc/pressure/memory`, in microseconds.
pub fn psi_some_total(text: &str) -> Option<u64> {
    text.lines()
        .find(|l| l.starts_with("some "))?
        .split_whitespace()
        .find_map(|w| w.strip_prefix("total="))?
        .parse()
        .ok()
}

/// A field of a cgroup's `memory.stat`.
pub fn stat(text: &str, name: &str) -> u64 {
    text.lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.trim().parse().ok())
        .unwrap_or(0)
}

/// File pages on the LRU: what `swappiness=0` reclaim can take. Not `file`,
/// which counts tmpfs and shared memory, and those need swap to go.
pub fn file_lru(memory_stat: &str) -> u64 {
    stat(memory_stat, "inactive_file") + stat(memory_stat, "active_file")
}

/// What the host and the loop share. Counters are since boot.
pub struct Shared {
    gain: AtomicU32,
    /// Seconds since `EPOCH` at which the gain was last said; 0 is never.
    gain_at: AtomicU64,
    /// Reclaim this agent asked for outside the loop (the host's `reclaim`
    /// requests), to be discounted from the next period's stall.
    own_usec: AtomicU64,
    periods: AtomicU64,
    paused: AtomicU64,
    reclaimed: AtomicU64,
    compactions: AtomicU64,
    last_some_ppm: AtomicU64,
    last_step: AtomicU64,
    running: AtomicU32,
}

pub static SHARED: Shared = Shared {
    gain: AtomicU32::new(1),
    gain_at: AtomicU64::new(0),
    own_usec: AtomicU64::new(0),
    periods: AtomicU64::new(0),
    paused: AtomicU64::new(0),
    reclaimed: AtomicU64::new(0),
    compactions: AtomicU64::new(0),
    last_some_ppm: AtomicU64::new(0),
    last_step: AtomicU64::new(0),
    running: AtomicU32::new(0),
};

static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn seconds() -> u64 {
    // From one: zero means the gain was never said.
    EPOCH.get_or_init(Instant::now).elapsed().as_secs() + 1
}

impl Shared {
    /// The host's word, from the control channel.
    pub fn set_gain(&self, gain: u32) {
        self.gain.store(gain.clamp(1, GAIN_MAX), Ordering::Relaxed);
        self.gain_at.store(seconds(), Ordering::Relaxed);
    }

    fn gain_now(&self) -> u32 {
        gain_at(
            self.gain.load(Ordering::Relaxed),
            self.gain_at.load(Ordering::Relaxed),
            seconds(),
        )
    }

    /// Reclaim done for the host, outside the loop: not the containers' stall.
    pub fn charge_own(&self, elapsed: Duration) {
        self.own_usec.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
    }

    /// One line for the control channel (`warm`), and from there the CLI.
    pub fn report(&self) -> String {
        let state = match self.running.load(Ordering::Relaxed) {
            0 => "off",
            1 => "on",
            _ => "no-psi",
        };
        format!(
            "warm {state} gain={} some_ppm={} step_kib={} periods={} paused={} reclaimed_mib={} compactions={}\n",
            self.gain_now(),
            self.last_some_ppm.load(Ordering::Relaxed),
            self.last_step.load(Ordering::Relaxed) >> 10,
            self.periods.load(Ordering::Relaxed),
            self.paused.load(Ordering::Relaxed),
            self.reclaimed.load(Ordering::Relaxed) >> 20,
            self.compactions.load(Ordering::Relaxed),
        )
    }
}

/// A gain said at `said` (0: never), read at `now`, both in seconds.
pub fn gain_at(gain: u32, said: u64, now: u64) -> u32 {
    if said == 0 || now.saturating_sub(said) > GAIN_TTL.as_secs() { 1 } else { gain }
}

/// A cgroup the loop tends, and the file cache it is left with.
pub struct Group {
    pub path: String,
    pub floor: u64,
}

/// Starts the loop on a thread of its own: a `memory.reclaim` write returns
/// when the reclaim is done, and the agent's tick must never wait for one.
pub fn start(groups: Vec<Group>, compact: fn()) {
    let spawned = std::thread::Builder::new().name("warm".into()).spawn(move || {
        run(&groups, "/proc/pressure/memory", compact, &SHARED, |d| std::thread::sleep(d))
    });
    if let Err(e) = spawned {
        println!("AGENT warm: no thread ({e})");
    }
}

fn run(groups: &[Group], psi: &str, compact: fn(), shared: &Shared, sleep: impl Fn(Duration)) {
    let read_total = || std::fs::read_to_string(psi).ok().and_then(|t| psi_some_total(&t));
    let Some(mut total) = read_total() else {
        shared.running.store(2, Ordering::Relaxed);
        println!("AGENT warm: off, no pressure stall information to stop it");
        return;
    };
    shared.running.store(1, Ordering::Relaxed);
    let mut at = Instant::now();
    let mut own = Duration::ZERO;
    let mut since_compaction = 0u64;
    let (mut hour_periods, mut hour_paused, mut hour_reclaimed) = (0u64, 0u64, 0u64);
    loop {
        sleep(PERIOD);
        let Some(now_total) = read_total() else { return };
        let now = Instant::now();
        let own_usec = own.as_micros() as u64 + shared.own_usec.swap(0, Ordering::Relaxed);
        let some = some_ppm(
            now_total.saturating_sub(total),
            own_usec,
            now.duration_since(at).as_micros() as u64,
        );
        (total, at, own) = (now_total, now, Duration::ZERO);
        let gain = shared.gain_now();
        shared.periods.fetch_add(1, Ordering::Relaxed);
        shared.last_some_ppm.store(some, Ordering::Relaxed);
        let paused = some >= THRESHOLD_PPM;
        if paused {
            shared.paused.fetch_add(1, Ordering::Relaxed);
            hour_paused += 1;
        }
        hour_periods += 1;
        if hour_periods >= REPORT_EVERY {
            if hour_reclaimed >> 20 > 0 {
                println!(
                    "AGENT warm: returned {} MiB of idle cache in the last hour, held by the guest's stall {hour_paused} of {hour_periods} periods, gain {gain}",
                    hour_reclaimed >> 20
                );
            }
            (hour_periods, hour_paused, hour_reclaimed) = (0, 0, 0);
        }
        let mut asked = 0u64;
        for group in groups {
            let read = || std::fs::read_to_string(format!("{}/memory.stat", group.path)).map(|s| file_lru(&s));
            let Ok(before) = read() else { continue };
            let step = step(before, group.floor, gain, some);
            if step == 0 {
                continue;
            }
            asked += step;
            let began = Instant::now();
            // EAGAIN when less came back than was asked: what the kernel
            // could not take it will be asked for again next period, smaller.
            let _ = std::fs::write(format!("{}/memory.reclaim", group.path), format!("{step} swappiness=0"));
            own += began.elapsed();
            let freed = before.saturating_sub(read().unwrap_or(before));
            shared.reclaimed.fetch_add(freed, Ordering::Relaxed);
            since_compaction += freed;
            hour_reclaimed += freed;
        }
        shared.last_step.store(asked, Ordering::Relaxed);
        if gain > 1 && since_compaction >= COMPACT_EVERY {
            since_compaction = 0;
            let began = Instant::now();
            compact();
            own += began.elapsed();
            shared.compactions.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn the_step_is_metas_at_gain_one_and_no_stall() {
        // 0.0005 of 8 GiB is 4 MiB, to a page.
        assert_eq!(step(8 * GIB, 0, 1, 0), (8 * GIB / 2000) & !4095);
    }

    #[test]
    fn stall_shrinks_the_step_and_the_threshold_stops_it() {
        let full = step(8 * GIB, 0, 8, 0);
        let half = step(8 * GIB, 0, 8, THRESHOLD_PPM / 2);
        assert!(half * 2 <= full + 8192 && half * 2 + 8192 >= full, "{half} {full}");
        assert_eq!(step(8 * GIB, 0, 8, THRESHOLD_PPM), 0);
        assert_eq!(step(8 * GIB, 0, GAIN_MAX, 50_000), 0);
    }

    #[test]
    fn no_gain_passes_a_hundredth_of_the_cache() {
        assert_eq!(GAIN_MAX, 20);
        assert_eq!(step(8 * GIB, 0, GAIN_MAX, 0), (8 * GIB / 100) & !4095);
        assert_eq!(step(8 * GIB, 0, 1000, 0), step(8 * GIB, 0, GAIN_MAX, 0));
        // A gain of nothing is a gain of one: the host cannot switch it off
        // by accident, only the command line can.
        assert_eq!(step(8 * GIB, 0, 0, 0), step(8 * GIB, 0, 1, 0));
    }

    #[test]
    fn the_floor_is_left_and_small_steps_are_not_taken() {
        assert_eq!(step(GIB, GIB, 20, 0), 0);
        assert_eq!(step(GIB / 2, GIB, 20, 0), 0);
        // A hundredth of a cache just over its floor would go under it.
        assert_eq!(step(GIB + (1 << 20), GIB, 20, 0), 1 << 20);
        // 0.0005 of 256 MiB is 128 KiB: under the smallest step worth a trip.
        assert_eq!(step(256 << 20, 0, 1, 0), 0);
        assert!(step(256 << 20, 0, 8, 0) >= MIN_STEP);
    }

    #[test]
    fn the_loops_own_reclaim_is_not_the_guests_stall() {
        let period = PERIOD.as_micros() as u64;
        // 9 ms stalled in six seconds is 1500 ppm, over the threshold; 5 ms
        // of it was this agent's own reclaim, and 4 ms is 666 ppm, under it.
        assert_eq!(some_ppm(9_000, 0, period), 1500);
        assert_eq!(some_ppm(9_000, 5_000, period), 666);
        assert_eq!(some_ppm(3_000, 5_000, period), 0);
        assert_eq!(some_ppm(1, 0, 0), THRESHOLD_PPM);
    }

    #[test]
    fn a_gain_nobody_refreshes_is_one() {
        assert_eq!(gain_at(8, 0, 500), 1);
        assert_eq!(gain_at(8, 100, 100 + GAIN_TTL.as_secs()), 8);
        assert_eq!(gain_at(8, 100, 101 + GAIN_TTL.as_secs()), 1);
    }

    #[test]
    fn the_kernels_files_are_read_as_they_are_written() {
        let psi = "some avg10=0.00 avg60=0.12 avg300=0.05 total=45013281\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=43589981\n";
        assert_eq!(psi_some_total(psi), Some(45_013_281));
        assert_eq!(psi_some_total("full avg10=0.00 total=1\n"), None);
        let stat = "anon 100\nfile 900\nshmem 300\ninactive_file 250\nactive_file 350\nfile_mapped 5\nworkingset_refault_file 7\n";
        // 900 of file less 300 of shmem; never `file_mapped`'s prefix.
        assert_eq!(file_lru(stat), 600);
        assert_eq!(stat_of(stat, "file"), 900);
        assert_eq!(stat_of(stat, "workingset_refault_file"), 7);
        assert_eq!(stat_of(stat, "absent"), 0);
    }

    fn stat_of(text: &str, name: &str) -> u64 {
        stat(text, name)
    }

    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let path = std::env::temp_dir().join(format!("lighter-warm-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }
        fn file(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn shared() -> Shared {
        Shared {
            gain: AtomicU32::new(1),
            gain_at: AtomicU64::new(0),
            own_usec: AtomicU64::new(0),
            periods: AtomicU64::new(0),
            paused: AtomicU64::new(0),
            reclaimed: AtomicU64::new(0),
            compactions: AtomicU64::new(0),
            last_some_ppm: AtomicU64::new(0),
            last_step: AtomicU64::new(0),
            running: AtomicU32::new(0),
        }
    }

    fn noop() {}

    #[test]
    fn without_pressure_stall_information_the_loop_never_reclaims() {
        let dir = Dir::new("nopsi");
        std::fs::write(dir.file("memory.stat"), "inactive_file 8589934592\nactive_file 0\n").unwrap();
        let shared = shared();
        run(
            &[Group { path: dir.0.to_string_lossy().into_owned(), floor: 0 }],
            &dir.file("absent"),
            noop,
            &shared,
            |_| panic!("the loop slept: it is running with nothing to stop it"),
        );
        assert!(shared.report().starts_with("warm no-psi "), "{}", shared.report());
        assert!(!std::path::Path::new(&dir.file("memory.reclaim")).exists());
    }

    #[test]
    fn a_quiet_guest_is_asked_and_a_stalling_one_is_not() {
        let dir = Dir::new("loop");
        let psi = dir.file("pressure");
        std::fs::write(dir.file("memory.stat"), "inactive_file 8589934592\nactive_file 0\n").unwrap();
        std::fs::write(&psi, "some avg10=0.00 total=1000\n").unwrap();
        let shared = shared();
        shared.set_gain(8);
        let period = std::cell::Cell::new(0);
        let asked = std::cell::RefCell::new(Vec::new());
        let reclaim = dir.file("memory.reclaim");
        run(
            &[Group { path: dir.0.to_string_lossy().into_owned(), floor: 0 }],
            &psi,
            noop,
            &shared,
            |_| {
                if period.get() > 0 {
                    asked.borrow_mut().push(std::fs::read_to_string(&reclaim).unwrap_or_default());
                    let _ = std::fs::remove_file(&reclaim);
                }
                period.set(period.get() + 1);
                match period.get() {
                    // Quiet; then a period with a full second stalled; then
                    // the file gone, which ends the loop.
                    1 => {}
                    2 => std::fs::write(&psi, "some avg10=9.00 total=1001000\n").unwrap(),
                    _ => std::fs::remove_file(&psi).unwrap(),
                }
            },
        );
        let asked = asked.into_inner();
        // 0.004 of 8 GiB, file only; and then nothing at all.
        assert_eq!(asked, [format!("{} swappiness=0", (8 * GIB / 250) & !4095), String::new()]);
        assert_eq!(shared.paused.load(Ordering::Relaxed), 1);
        assert_eq!(shared.periods.load(Ordering::Relaxed), 2);
    }
}
