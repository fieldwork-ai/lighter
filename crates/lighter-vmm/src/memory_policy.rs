//! Deciding how much memory to ask the guest for.
//!
//! Two mechanisms return memory, and they answer different questions.
//!
//! **Free page reporting** is the guest volunteering: its allocator tells us
//! about runs it is not using, continuously and with nobody asking, and we hand
//! those pages back to macOS. This is what makes a build's memory disappear
//! after the build, and it needs no policy at all.
//!
//! **The balloon** is the host insisting. It only matters when the Mac itself
//! is short — and then it matters a great deal, because the alternative is
//! macOS compressing and swapping a guest's pages, which is far more expensive
//! than the guest simply not having them.
//!
//! macOS's three pressure levels set how far and how fast the balloon may
//! be asked to grow. A periodic sample of compression activity moves it
//! before those levels rise.
//! They are one ramp: the
//! level raises its cap and its step, the compressor moves it, and the quiet
//! rule brings it down. A guest that is short holds the ramp at what the
//! balloon already has: a guest with no memory to spare cannot give a
//! quarter of it, and a target past what it can give had its driver retrying
//! the allocation five times a second, each try a reclaim pass on a guest
//! with nothing left to reclaim, until Docker stopped answering (a defect
//! report of 2026-09-14: a build bounded at 10 GiB of a 16 GiB guest when
//! the Mac reported Warn). The ramp moves again once the guest has said it
//! is fine for a few seconds.
//!
//! Down is a ramp too, and the host's own numbers count. A guest asking
//! for its balloon back gets it a 32nd of RAM a second, each second's step
//! no more than half of what the host has free, because 4 GiB handed back
//! in one step to a Mac that had been at Warn sixteen seconds earlier had
//! the Mac paging the guest for thirty seconds: every vCPU and the stream
//! reactor faulting into the compressor, an RCU stall, and a build's
//! session lost (2026-09-20). A Warn is remembered for a minute, and a
//! compressor holding a quarter of RAM or swap half used is pressure
//! whatever level the Mac reports, since it reports Normal for seconds at
//! a time while deeply overcommitted.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::mempressure::{Observer, Pressure, Watcher};
use crate::virtio::balloon::{BALLOON_PAGE_SIZE, BalloonState};
use crate::virtio::mmio::VirtioMmio;

/// How much of guest RAM the ramp may reach at each level, and how fast.
///
/// Deliberately not aggressive at `Warn`: the system reclaiming is normal on a
/// Mac and happens long before anything is in trouble, so taking half the
/// guest's memory then would make every browser tab a stutter in the
/// container. `Critical` means macOS is about to swap or kill something, and at
/// that point a slow guest is much better than a wedged host.
///
/// A level raises the cap and the step of the one ramp; it never sets a
/// target of its own. A target that jumped to a quarter of RAM had the guest
/// hunting for all of it at once (a burst of failed allocations on every
/// Warn), and a target that fell to nothing the moment the level read Normal
/// had the guest refilling two gigabytes of cache the host then compressed
/// again, a minute later, eighteen times in half an hour (defect observation
/// of 2026-09-16). At an eighth of the distance a second a level is reached
/// in about eight seconds; coming down is the ramp's own quiet rule.
const WARN_FRACTION: u64 = 4; // cap: a quarter
const CRITICAL_FRACTION: u64 = 2; // cap: a half
const WARN_STEP_FRACTION: u64 = 32;
const CRITICAL_STEP_FRACTION: u64 = 16;

/// The pressure levels are the floor of the policy, not the whole of it.
/// An 8 GB Mac with a 4 GiB guest whose page cache had filled with three
/// package trees reported `Normal` the whole way through while its
/// compressor grew from 600 MB to 2 GB eating the guest's pages, and the
/// install after the big one paid fifteen percent. macOS compresses first
/// and reports pressure later.
///
/// Every cure that took the cache from outside the guest cost more than it
/// cured: a reclaim request over the control channel whenever the
/// compressor moved doubled the install, one only under distress still
/// fired eight times through two installs and made the first repetition of
/// each take twice as long, and a bound on the cache, throttled or
/// reclaimed, had the same shape. An install's working set is the cache,
/// and anything that takes a gigabyte of it at once is paid on every page.
///
/// The balloon is the one channel that lets the guest choose: inflate it a
/// step, and the guest's own LRU gives up its coldest pages to fill it,
/// which is the share's stale cache long before it is anything an install
/// is using. It failed the first time only because a 4 KiB guest page is
/// not a 16 KiB host page, so nothing inflated was ever released — patch
/// 0014 has the balloon inflate in host-page units. So the compressor is
/// the signal (pages being compressed is the exact cost this exists to
/// avoid), and the balloon target rises a small step each second while
/// the host is compressing and eases by the same step once compression
/// has stopped for five seconds. The quiet interval keeps the guest's
/// cache from immediately refilling the host. The pressure level remains a
/// minimum. The step scales with the memory currently plugged into the
/// guest: a fixed 32 MiB step takes 96 seconds to reach the steering cap
/// on a 24 GiB guest, during which the host can swap gigabytes of cache.
/// At one 256th per poll it takes at most 32 seconds at any guest size.
const COMPRESSING_BYTES_PER_POLL: u64 = 64 << 20;
const STEP_MIN_BYTES: u64 = 32 << 20;
const STEP_FRACTION: u64 = 256;
/// With no host pressure the ramp never asks for more than this share of
/// guest RAM; the pressure levels raise it.
const STEER_CAP_FRACTION: u64 = 8;
const POLL: Duration = Duration::from_secs(1);

/// The guest port the agent's control channel listens on.
pub const AGENT_CONTROL_PORT: u32 = 2376;
/// The host port the agent dials to say what it can spare (`memory_guest`).
pub const MEMORY_PORT: u32 = 2381;
/// The guest keeps at least this fraction of its RAM out of the balloon.
const GUEST_RESERVE_FRACTION: u64 = 16;
/// A held ramp moves again once the guest has been fine for this long: the
/// ramp's own patience before it deflates, so a build settling at its
/// reserve is not squeezed on every second it dips under the line and
/// recovers. Time, not lines: the agent sends a line only when its numbers
/// change, so an idle guest is silent, and silence is no verdict.
const HOLD_PATIENCE: Duration = Duration::from_secs(5);
/// How long a Warn or Critical is remembered: a Normal within it is the
/// level bouncing, not the host recovered, and a guest asking for its
/// memory back then is answered as it would be under pressure. On an
/// overcommitted Mac the level read Normal for sixteen seconds between two
/// Warns while 53 GB sat compressed (2026-09-20).
const PRESSURE_MEMORY: Duration = Duration::from_secs(60);
/// A guest asking for its balloon back, or short of memory, gets it down a
/// ramp, never in one step: a 32nd of RAM a second, the Warn ramp's pace,
/// so a balloon that took eight seconds to inflate takes eight to give
/// back. The 4 GiB that went back in one step at 13:41 on 2026-09-20 had
/// the host paging the guest for thirty seconds.
const RELEASE_STEP_FRACTION: u64 = 32;
/// A guest whose every task is stalled on memory for this share of the
/// last ten seconds (hundredths of a percent) comes down the release ramp
/// two steps a poll rather than one: the pacing is for the host's sake,
/// and a guest that is not running at all is the worse of the two.
const STALL_FULL_HURRY: u32 = 1000;
/// A host is overcommitted when its compressor holds more than this share
/// of RAM: memory the guest is handed then comes out of other processes'
/// compressed pages, whatever level the Mac reports.
const OVERCOMMITTED_COMPRESSOR_FRACTION: u64 = 4;
/// Swap in use over this fraction of the Mac's RAM is pressure. Against
/// RAM, not against the swap files' size: macOS grows and shrinks those to
/// what is in use, so used-over-total read 83% on a Mac that had drained
/// its compressor by half, and kept the balloon pinned for nothing.
const OVERCOMMITTED_SWAP_FRACTION: u64 = 8;
/// Overcommitment ends only once the compressor or swap is under this
/// fraction of RAM, a fifth below where it began: an 8 GB Mac sat within a
/// few megabytes of the quarter for a minute and read as overcommitted and
/// not eight times (the M1, 2026-09-20 22:50Z), each flip resetting the
/// ramp's patience.
const OVERCOMMITTED_RELEASE_FRACTION_NUM: u64 = 5;
/// An inflation that has made next to no progress toward its target for
/// this long is one the guest cannot make: its driver is retrying an
/// allocation that fails, five times a second, each try a reclaim pass that
/// may yield a page or two, so "next to no" rather than "no". A guest with
/// the memory inflates at gigabytes a second; the retries yield at most a
/// few hundred kilobytes a window. Seen from here, not reported by the
/// guest: in the defect report the agent never disconnected, it just
/// stopped being scheduled, and its last line had said it was fine.
const INFLATION_STALL: Duration = Duration::from_secs(3);
const STALL_PROGRESS_BYTES: u64 = 4 << 20;

/// The balloon, and the signals that drive it.
pub struct MemoryPolicy {
    /// Held for the machine's lifetime; dropping it stops the subscription.
    _watcher: Watcher,
    steering: Arc<Steering>,
    stop: Arc<AtomicBool>,
}

impl MemoryPolicy {
    /// Starts watching, and steering.
    pub fn start(
        balloon: Arc<BalloonState>,
        transport: Arc<Mutex<VirtioMmio>>,
        ram_bytes: u64,
        vsock: Arc<crate::virtio::vsock::VsockShared>,
    ) -> Result<MemoryPolicy, String> {
        let steering = Arc::new(Steering {
            balloon,
            transport,
            ram_bytes,
            vsock: Some(vsock.clone()),
            reclaiming: Arc::new(AtomicBool::new(false)),
            warm_gain: Arc::new(Mutex::new(None)),
            saying_gain: Arc::new(AtomicBool::new(false)),
            level: AtomicU32::new(Pressure::Normal as u32),
            compression: Mutex::new(CompressionState::default()),
            apply_lock: Mutex::new(()),
            guest_pages: AtomicU32::new(0),
        });
        memory_guest(vsock, steering.clone())?;
        let watcher = Watcher::start(Box::new(Levels(steering.clone())))?;
        let stop = Arc::new(AtomicBool::new(false));
        // A fixed target, for measuring what the guest gives back and what
        // the host can take of it, with no host pressure involved.
        if let Some(mib) = std::env::var("LIGHTER_BALLOON_TEST_MIB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            steering
                .compression
                .lock()
                .expect("compression policy poisoned")
                .pages = ((mib << 20) / BALLOON_PAGE_SIZE) as u32;
            steering.apply();
        }
        if steering_enabled() {
            let host = HostMemory::new();
            let steering = steering.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("memory-policy".into())
                .spawn(move || {
                    let mut last = host.sample();
                    // `LIGHTER_MEM_TRACE=1`: what the guest has reported free
                    // and what the host holds, every second, to the log.
                    let trace = std::env::var("LIGHTER_MEM_TRACE").map(|v| v == "1").unwrap_or(false);
                    // `LIGHTER_PRESSURE_TEST_FILE`: a file naming a level
                    // (normal, warn, critical), read every poll and applied
                    // as if macOS had reported it, for a test that needs the
                    // event at a moment of its choosing. The real watcher
                    // stays subscribed, so this is for a quiet host only.
                    let pressure_test = std::env::var_os("LIGHTER_PRESSURE_TEST_FILE");
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(POLL);
                        steering.inflation_stalled(Instant::now());
                        if let Some(path) = &pressure_test
                            && let Ok(text) = std::fs::read_to_string(path)
                        {
                            let level = match text.trim() {
                                "normal" => Some(Pressure::Normal),
                                "warn" => Some(Pressure::Warn),
                                "critical" => Some(Pressure::Critical),
                                _ => None,
                            };
                            if let Some(level) = level {
                                Levels(steering.clone()).pressure(level);
                            }
                        }
                        // Sampled every second whether traced or not: the
                        // release path decides from it (`memory::release`).
                        let (resident, internal, reusable, compressed) = crate::footprint::sample();
                        if trace {
                            eprintln!(
                                "MEMTRACE footprint_mib={} resident_mib={} internal_mib={} reusable_mib={} compressed_mib={} reported_mib={} offered_mib={} steer_mib={} level_mib={}",
                                crate::footprint::bytes() >> 20,
                                resident >> 20,
                                internal >> 20,
                                reusable >> 20,
                                compressed >> 20,
                                steering.balloon.reported_bytes() >> 20,
                                steering.balloon.offered_bytes() >> 20,
                                (steering.compression.lock().expect("compression policy poisoned").pages as u64 * BALLOON_PAGE_SIZE) >> 20,
                                (u64::from(cap_pages(steering.level.load(Ordering::Relaxed), steering.total_bytes())) * BALLOON_PAGE_SIZE) >> 20
                            );
                        }
                        let Some(now) = host.sample() else { continue };
                        steering.observe(&now);
                        if let Some(then) = last {
                            steering.steer(now.compressed.saturating_sub(then.compressed));
                        }
                        last = Some(now);
                    }
                })
                .map_err(|e| format!("cannot start the memory policy thread: {e}"))?;
        }
        Ok(MemoryPolicy {
            _watcher: watcher,
            steering,
            stop,
        })
    }

    /// The last level the host reported. Diagnostics.
    pub fn level(&self) -> u32 {
        self.steering.level.load(Ordering::Relaxed)
    }
}

impl Drop for MemoryPolicy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// `LIGHTER_MEMORY_STEER=0` leaves the balloon to the pressure levels
/// alone, for measuring what the steering costs against what it saves.
fn steering_enabled() -> bool {
    std::env::var("LIGHTER_MEMORY_STEER")
        .ok()
        .is_none_or(|v| v != "0")
}

/// The ramp, and what holds it.
#[derive(Default)]
struct CompressionState {
    /// The ramp's target, in balloon pages: stepped by the poll, frozen by
    /// a hold, withdrawn by the guest.
    pages: u32,
    /// The guest is short: its `need` line, a `release` with no host
    /// pressure, an inflation that stalled, or its agent gone.
    guest_short: bool,
    /// When the guest last became fine; none until it has said so, so the
    /// ramp waits for the guest to speak before it takes anything.
    healthy_since: Option<Instant>,
    /// While the balloon is short of its target: what it held, and when it
    /// last grew.
    inflation: Option<(u32, Instant)>,
    /// Polls in a row with the host compressing nothing. A level change
    /// restarts it: a level that just dropped gets a few seconds' grace
    /// before the ramp eases, so a host flapping between Warn and Normal
    /// does not have the balloon eased on every Normal second.
    quiet_polls: u32,
    /// Whether the last `apply` reported the ramp held, so the transitions
    /// are logged once each way rather than every poll.
    hold_reported: bool,
    /// The guest asked for its balloon back with no host pressure: the
    /// ramp comes down a step a poll until it is gone or the guest says
    /// otherwise.
    releasing: bool,
    /// What the host has free, from the last sample; deflation is paced by
    /// it. All of it until the first sample.
    host_free: Option<u64>,
    /// Whether the host's compressor or swap says it is overcommitted,
    /// whatever level it reports.
    overcommitted: bool,
    /// Until when a Warn or Critical is remembered.
    pressure_until: Option<Instant>,
    /// The guest's memory stall averages over ten seconds, hundredths of a
    /// percent: some task stalled, every task stalled.
    stall_some: u32,
    stall_full: u32,
}

impl CompressionState {
    /// Whether the ramp is held at what the balloon has: the guest is
    /// short, or has not been fine for long enough yet.
    fn held(&self, now: Instant) -> bool {
        self.guest_short
            || self
                .healthy_since
                .is_none_or(|since| now.duration_since(since) < HOLD_PATIENCE)
    }

    /// Whether the host is under pressure by any of its signs: the level,
    /// a level within the last minute, or its compressor and swap.
    fn pressed(&self, level: u32, now: Instant) -> bool {
        level != Pressure::Normal as u32
            || self.overcommitted
            || self.pressure_until.is_some_and(|until| now < until)
    }
}

struct Steering {
    balloon: Arc<BalloonState>,
    transport: Arc<Mutex<VirtioMmio>>,
    /// What the guest has: its whole RAM, one zone (0.7.2).
    ram_bytes: u64,
    /// The way to the guest's agent, for a reclaim request ahead of the
    /// balloon; none in the tests.
    vsock: Option<Arc<crate::virtio::vsock::VsockShared>>,
    /// A reclaim request in flight: one at a time.
    reclaiming: Arc<AtomicBool>,
    /// The warm loop's gain as last said to the agent, and when.
    warm_gain: Arc<Mutex<Option<(u32, Instant)>>>,
    /// A gain on its way to the agent: one at a time.
    saying_gain: Arc<AtomicBool>,
    /// The host's pressure level: the ramp's cap and step.
    level: AtomicU32,
    // Keep the ramp and its hold together: a poll must not restore a
    // target withdrawn concurrently.
    compression: Mutex<CompressionState>,
    // Publish targets in the same order they are computed from current state.
    apply_lock: Mutex<()>,
    guest_pages: AtomicU32,
}

impl Steering {
    /// What the guest has, which is what the fractions are of.
    fn total_bytes(&self) -> u64 {
        self.ram_bytes
    }

    /// The poll, with what the host compressed since the last one: the
    /// ramp moves unless held. A held ramp is frozen where the balloon is
    /// by `apply`.
    fn steer(&self, compressed: u64) {
        let gain;
        {
            let mut state = self
                .compression
                .lock()
                .expect("compression policy poisoned");
            let now = Instant::now();
            let total = self.total_bytes();
            state.quiet_polls = if compressed == 0 {
                state.quiet_polls.saturating_add(1)
            } else {
                0
            };
            let level = self.level.load(Ordering::Relaxed);
            gain = warm_gain(
                level,
                state.pressed(level, now),
                state.quiet_polls < QUIET_POLLS_BEFORE_DEFLATE,
            );
            // An overcommitted host, or one that was at Warn a moment ago,
            // is steered as at Warn whatever it reports now; and it is not
            // quiet, so the grace before easing counts from when that ends.
            let level = if level == Pressure::Normal as u32 && state.pressed(level, now) {
                state.quiet_polls = 0;
                Pressure::Warn as u32
            } else {
                level
            };
            if state.releasing {
                // Down the ramp a step a poll, whatever else holds it;
                // `apply` paces each step by what the host has free. Two
                // steps while the guest's every task is stalled on memory.
                let mut step = as_pages((total / RELEASE_STEP_FRACTION).max(STEP_MIN_BYTES));
                if state.stall_full >= STALL_FULL_HURRY {
                    step = step.saturating_mul(2);
                }
                state.pages = state.pages.saturating_sub(step);
                if state.pages == 0 {
                    state.releasing = false;
                }
            } else if !state.held(now) {
                let before = state.pages;
                state.pages = steer(state.pages, compressed, state.quiet_polls, total, level);
                // Cooperative first: a step up under pressure is asked of
                // the guest as a reclaim of the same amount from the
                // containers' cgroup, coldest cache first, before the
                // balloon takes it. The kernel's LRU picks the victims;
                // the balloon then pins pages that are already free.
                if state.pages > before && level != Pressure::Normal as u32 {
                    let step = u64::from(state.pages - before) * BALLOON_PAGE_SIZE;
                    self.ask_reclaim(step >> 20);
                }
            }
        }
        self.say_gain(warm_gain_forced().unwrap_or(gain));
        self.apply();
    }

    /// Tells the agent's warm loop how hard to push, when that changes and
    /// once a minute besides, off the poll's thread like a reclaim request
    /// and one at a time. Said is said only once the agent has answered: the
    /// first poll comes before the agent listens, and a gain recorded then
    /// left a guest at 1 for its first minute under a Mac at Warn. An agent
    /// that predates the loop answers with an error, which is an answer.
    fn say_gain(&self, gain: u32) {
        let Some(vsock) = &self.vsock else { return };
        {
            let said = self.warm_gain.lock().expect("warm gain poisoned");
            if said.is_some_and(|(g, at)| g == gain && at.elapsed() < WARM_GAIN_REFRESH) {
                return;
            }
        }
        if self
            .saying_gain
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let (vsock, said, saying) = (
            vsock.clone(),
            self.warm_gain.clone(),
            self.saying_gain.clone(),
        );
        let spawned = std::thread::Builder::new()
            .name("warm-gain".into())
            .spawn(move || {
                match ask_agent(&vsock, &format!("gain {gain}\n")) {
                    Ok(_) => {
                        let mut said = said.lock().expect("warm gain poisoned");
                        if said.is_none_or(|(g, _)| g != gain) {
                            tracing::info!(gain, "warm loop gain");
                        }
                        *said = Some((gain, Instant::now()));
                    }
                    Err(e) => tracing::debug!(gain, %e, "the agent did not take the warm gain"),
                }
                saying.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.saying_gain.store(false, Ordering::Release);
        }
    }

    /// Asks the agent to reclaim `mib` from the containers' cgroup
    /// (`memory.reclaim`), one request at a time, off the poll's thread. The
    /// agent answers with what came back; the line is logged, and the
    /// balloon's next step finds the pages free.
    fn ask_reclaim(&self, mib: u64) {
        let Some(vsock) = &self.vsock else { return };
        if !reclaim_ahead_enabled() {
            return;
        }
        if mib == 0
            || self
                .reclaiming
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let vsock = vsock.clone();
        let reclaiming = self.reclaiming.clone();
        let spawned = std::thread::Builder::new()
            .name("memory-reclaim".into())
            .spawn(move || {
                let answer = ask_agent(&vsock, &format!("reclaim {mib}\n"));
                match answer {
                    Ok(line) => tracing::info!(asked_mib = mib, %line, "the guest reclaimed ahead of the balloon"),
                    Err(e) => tracing::debug!(asked_mib = mib, %e, "no reclaim answer from the agent"),
                }
                reclaiming.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.reclaiming.store(false, Ordering::Release);
        }
    }

    /// The host's own numbers, each poll: what it has free paces
    /// deflation, and a full compressor or half-used swap is pressure
    /// whatever level it reports.
    fn observe(&self, sample: &HostSample) {
        let mut state = self
            .compression
            .lock()
            .expect("compression policy poisoned");
        state.host_free = Some(sample.free);
        // In at a quarter (an eighth for swap), out a fifth below that:
        // hysteresis against a host sitting on the line.
        let line = |fraction: u64| {
            let on = sample.ram / fraction;
            if state.overcommitted {
                on - on / OVERCOMMITTED_RELEASE_FRACTION_NUM
            } else {
                on
            }
        };
        let compressor_full =
            sample.ram > 0 && sample.compressor > line(OVERCOMMITTED_COMPRESSOR_FRACTION);
        let swapping = sample.ram > 0 && sample.swap_used > line(OVERCOMMITTED_SWAP_FRACTION);
        let overcommitted = compressor_full || swapping;
        if overcommitted != state.overcommitted {
            tracing::info!(
                overcommitted,
                compressor_mib = sample.compressor >> 20,
                swap_used_mib = sample.swap_used >> 20,
                swap_total_mib = sample.swap_total >> 20,
                free_mib = sample.free >> 20,
                "host overcommitment changed"
            );
        }
        state.overcommitted = overcommitted;
    }

    /// The guest's memory stall averages, from its line: kept for the ramp's
    /// pace and said when they change by a whole percent.
    fn guest_stall(&self, some: u32, full: u32) {
        let mut state = self
            .compression
            .lock()
            .expect("compression policy poisoned");
        if some / 100 != state.stall_some / 100 || full / 100 != state.stall_full / 100 {
            tracing::debug!(
                some_pct = some / 100,
                full_pct = full / 100,
                "the guest's memory stall changed"
            );
        }
        state.stall_some = some;
        state.stall_full = full;
    }

    /// A line from the guest. `need` (busy, under an eighth available) holds
    /// the ramp where the balloon is, at any level: deflate-on-OOM and
    /// renewed inflation would otherwise recycle the same pages while an
    /// oversized build made little progress. Since 0.7.2 a need also
    /// brings the ramp down its paced steps at any level: a hold alone
    /// left the guest starved under a host that read as overcommitted
    /// (the M5, 2026-09-20 19:16Z), and each step is paced by what the
    /// host has free, so the Mac is never handed the cliff back. `release`
    /// (active work, free memory under half the reserve) is the guest
    /// asking for the reserve back: with no host pressure the ramp comes
    /// down; under host pressure it is ignored, because inflation itself
    /// keeps free memory low while the guest still has gigabytes of cold
    /// cache to give — a hold on it kept two gigabytes back from a host
    /// that was swapping, and the host asked again a minute later.
    fn guest_demand(&self, need: bool, release: bool) {
        let now = Instant::now();
        let normal = {
            let state = self
                .compression
                .lock()
                .expect("compression policy poisoned");
            !state.pressed(self.level.load(Ordering::Relaxed), now)
        };
        // A need at any level, or a release with no host pressure, brings
        // the ramp down its paced steps; a release under pressure is
        // ignored, since inflation itself keeps free memory low while the
        // guest still has cold cache to give.
        let short = need || (release && normal);
        self.guest_short(short, short, now);
    }

    /// The balloon's own progress, from the policy's poll: a target the
    /// guest has moved toward by less than `STALL_PROGRESS_BYTES` in
    /// `INFLATION_STALL` is one it cannot reach, and the guest is short
    /// whatever its last line said. The hold lasts until the guest's next
    /// fine line: a guest too starved to send one is exactly the guest this
    /// is for.
    fn inflation_stalled(&self, now: Instant) {
        let (target, actual) = (self.balloon.target_pages(), self.balloon.actual_pages());
        let progress = (STALL_PROGRESS_BYTES / BALLOON_PAGE_SIZE) as u32;
        let stalled = {
            let mut state = self
                .compression
                .lock()
                .expect("compression policy poisoned");
            if target <= actual {
                state.inflation = None;
                false
            } else if let Some((seen, since)) = state.inflation
                && actual.saturating_sub(seen) < progress
            {
                !state.guest_short && now.duration_since(since) >= INFLATION_STALL
            } else {
                state.inflation = Some((actual, now));
                false
            }
        };
        if stalled {
            tracing::info!(
                target_mib = (u64::from(target) * BALLOON_PAGE_SIZE) >> 20,
                actual_mib = (u64::from(actual) * BALLOON_PAGE_SIZE) >> 20,
                "balloon inflation made under {} MiB of progress in {}s: the guest cannot give it",
                STALL_PROGRESS_BYTES >> 20,
                INFLATION_STALL.as_secs()
            );
            self.guest_short(true, false, now);
        }
    }

    /// The guest is `short`, or no longer is; `withdraw` gives the ramp
    /// back entirely (the guest wants its reserve and the host is not
    /// short), otherwise a hold freezes it where the balloon is.
    fn guest_short(&self, short: bool, withdraw: bool, now: Instant) {
        let changed = {
            let mut state = self
                .compression
                .lock()
                .expect("compression policy poisoned");
            let held = state.held(now);
            if short {
                state.healthy_since = None;
            } else if state.healthy_since.is_none() || state.guest_short {
                state.healthy_since = Some(now);
            }
            let changed = state.guest_short != short;
            state.guest_short = short;
            // The guest wants its memory back and the host has it to give:
            // down the ramp from the next poll, never in one step.
            if withdraw && !state.releasing && state.pages > 0 {
                tracing::info!(
                    ramp_mib = (u64::from(state.pages) * BALLOON_PAGE_SIZE) >> 20,
                    stall_some_pct = state.stall_some / 100,
                    stall_full_pct = state.stall_full / 100,
                    "the guest asks for its memory back; the ramp comes down"
                );
            }
            if withdraw {
                state.releasing = true;
            } else if !short {
                state.releasing = false;
            }
            changed || withdraw || held != state.held(now)
        };
        if changed {
            tracing::debug!(short, withdraw, "the guest's line moved the ramp");
            self.apply();
        }
    }

    /// The guest says it can spare `spare_mib` beyond what the balloon
    /// already holds; `release` asks the whole balloon back. An offer under
    /// 64 MiB holds the target where it is. A step is at most 8 GiB, so
    /// the next second's offer is measured against inflation that has
    /// actually happened rather than a target still being filled — the
    /// balloon's own count of what it holds lags its allocations, and a
    /// target summed from the two overshoots the guest's reserve.
    fn guest_offers(&self, spare_mib: u64, release: bool) {
        const STEP_MIB: u64 = 8192;
        let total = self.total_bytes();
        let cap = ((total - total / GUEST_RESERVE_FRACTION) / BALLOON_PAGE_SIZE)
            .min(u64::from(u32::MAX)) as u32;
        let pages = if release {
            0
        } else if spare_mib < 64 {
            // Hold at what the balloon actually holds, not at the target it
            // was given: a target past what the guest could spare has the
            // driver retrying its last pages every 200 ms for good ("Out of
            // puff"), reclaiming a little each time.
            let actual = self.balloon.actual_pages();
            if actual >= self.guest_pages.load(Ordering::Relaxed) {
                return;
            }
            actual
        } else {
            // Never lower on an offer: the balloon's count of what it holds
            // lags its allocations, and a target summed from a stale count
            // fell below the one being filled — the driver deflated, the
            // next offer inflated, and two seconds went on the seesaw.
            let actual = u64::from(self.balloon.actual_pages());
            let wanted = (actual + (spare_mib.min(STEP_MIB) << 20) / BALLOON_PAGE_SIZE)
                .min(u64::from(cap))
                .min(u64::from(u32::MAX)) as u32;
            wanted.max(self.guest_pages.load(Ordering::Relaxed))
        };
        if pages != self.guest_pages.swap(pages, Ordering::Relaxed) {
            self.apply();
        }
    }

    fn apply(&self) {
        let _apply = self
            .apply_lock
            .lock()
            .expect("memory policy apply poisoned");
        let actual = self.balloon.actual_pages();
        let ramp = {
            let mut state = self
                .compression
                .lock()
                .expect("compression policy poisoned");
            // Re-evaluated on every poll (`steer` applies), which is how a
            // hold ends once the guest's patience has run. While held the
            // ramp is frozen where the balloon is, and follows it down if
            // deflate-on-OOM takes pages back: never re-inflated.
            let held = state.held(Instant::now());
            if held {
                state.pages = state.pages.min(actual);
            }
            if held != state.hold_reported {
                state.hold_reported = held;
                let mib = |pages: u32| (u64::from(pages) * BALLOON_PAGE_SIZE) >> 20;
                if held {
                    tracing::info!(
                        held_mib = mib(actual),
                        "balloon ramp held at what the balloon has: the guest is short of memory"
                    );
                } else {
                    tracing::info!(ramp_mib = mib(state.pages), "balloon ramp resumes");
                }
            }
            state.pages
        };
        let mut pages = ramp.max(self.guest_pages.load(Ordering::Relaxed));
        // Deflation paced by what the host has free: the guest is never
        // handed more a second than the host can give without paging, half
        // its free memory, and at least the small step.
        if pages < actual {
            let free = self
                .compression
                .lock()
                .expect("compression policy poisoned")
                .host_free;
            if let Some(free) = free {
                let allowed = as_pages((free / 2).max(STEP_MIN_BYTES));
                pages = pages.max(actual.saturating_sub(allowed));
            }
        }
        let before = self.balloon.target_pages();
        if pages == before {
            return;
        }
        self.balloon.set_target_pages(pages);
        // The guest only reads the target when told the configuration changed,
        // so this is not bookkeeping — it is the whole delivery mechanism.
        self.transport
            .lock()
            .expect("balloon transport poisoned")
            .notify_config_change();
        let mib = (u64::from(pages) * BALLOON_PAGE_SIZE) >> 20;
        // A step is routine; a crossing of a quarter gigabyte is worth a line.
        if ((u64::from(before) * BALLOON_PAGE_SIZE) >> 28)
            != ((u64::from(pages) * BALLOON_PAGE_SIZE) >> 28)
        {
            tracing::info!(target_mib = mib, "balloon target");
        } else {
            tracing::debug!(target_mib = mib, "balloon target");
        }
    }
}

/// The guest's own offer, and the third input to the balloon.
///
/// Free page reporting returns runs of two megabytes, and what a package
/// install frees is in file-sized pieces below that: traced through the
/// storage cases, the guest had 13 GB free with the host still holding
/// 6.4 GB of it, and only a compaction pass — which costs the next
/// command — made it reportable. The balloon needs no contiguity beyond a
/// host page. So the agent, which knows when its containers are idle and
/// how much is free, dials in and says each second what it can spare; the
/// host inflates by that much and deflates the moment the guest says zero
/// — work resumed, or free memory below its reserve. Sixteen bytes a
/// second: spare, available, free (MiB), and the idle count, all `u32`.
/// One line to the agent's control port and its first line back, over a
/// socket pair the device carries into the guest, as the CLI's control
/// socket is carried.
fn ask_agent(
    vsock: &Arc<crate::virtio::vsock::VsockShared>,
    line: &str,
) -> std::io::Result<String> {
    let (mine, theirs) = std::os::unix::net::UnixStream::pair()?;
    let host_port = vsock.open(AGENT_CONTROL_PORT, theirs);
    if !vsock.await_established(host_port, Duration::from_secs(4)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the agent did not accept",
        ));
    }
    // Both directions go through the device, not the pair: nothing forwards
    // what is written into a socket handed to `open`, and what the guest
    // says is queued on the connection, not written to it (a proxy's pump
    // does both for its client). 0.7.2 wrote the line into the pair and read
    // the pair for the answer, so no line ever left the Mac: every reclaim
    // asked of the guest ahead of the balloon timed out thirty seconds later
    // at debug level, and no machine ever logged one that worked. Found
    // 2026-09-21, when the warm loop's gain went the same way and the agent's
    // own report said it had heard nothing.
    let gone = || {
        std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "the agent's connection went",
        )
    };
    if !vsock.send(host_port, line.as_bytes()) {
        return Err(gone());
    }
    // A reclaim answers when the reclaim is done, which can be seconds.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut answer = Vec::new();
    let result = loop {
        match vsock.try_read_outbound(host_port, 1) {
            Ok(Some(byte)) if byte == *b"\n" => break Ok(()),
            Ok(Some(byte)) => answer.extend_from_slice(&byte),
            Ok(None) if Instant::now() >= deadline => {
                break Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the agent did not answer",
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break Err(gone()),
        }
    };
    vsock.shutdown(host_port);
    drop(mine);
    result?;
    Ok(String::from_utf8_lossy(&answer).trim().to_string())
}

fn memory_guest(
    vsock: Arc<crate::virtio::vsock::VsockShared>,
    steering: Arc<Steering>,
) -> Result<(), String> {
    let accepted = vsock.listen(MEMORY_PORT);
    // Each connection is read on its own thread, so a new one from the
    // agent is heard at once whatever became of the last: a guest at its
    // memory limit lost the connection's close on the way out (m6, the
    // M1, 2026-09-20 23:00Z), the one reader sat on the dead connection,
    // and the fresh line that would have ended the hold waited behind it
    // for good. The hold for a silent guest applies only when no
    // connection is alive.
    let live = Arc::new(AtomicU32::new(0));
    std::thread::Builder::new()
        .name("memory-guest".into())
        .spawn(move || {
            for crate::virtio::vsock::Accepted { key } in accepted {
                let vsock = vsock.clone();
                let steering = steering.clone();
                live.fetch_add(1, Ordering::AcqRel);
                let alive = live.clone();
                let spawned = std::thread::Builder::new()
                    .name("memory-line".into())
                    .spawn(move || {
                        while let Some(bytes) = vsock.read_outbound_exact(key, 32) {
                            let word = |i: usize| {
                                u32::from_le_bytes([
                                    bytes[i],
                                    bytes[i + 1],
                                    bytes[i + 2],
                                    bytes[i + 3],
                                ])
                            };
                            let spare_mib = u64::from(word(0));
                            let flags = word(12);
                            let (release, need) = (flags & 1 != 0, flags & 2 != 0);
                            // Pressure stall information, hundredths of a
                            // percent of the last ten seconds: `some` is the
                            // guest's own reason for a need; `full` paces how
                            // fast it is answered.
                            steering.guest_stall(word(16), word(20));
                            // The line drives the ramp and, when the guest
                            // has memory to spare, the balloon's own offer.
                            steering.guest_demand(need, release);
                            if release || need {
                                steering.guest_offers(0, true);
                            } else {
                                steering.guest_offers(spare_mib, false);
                            }
                        }
                        // Without current guest feedback the guest is short
                        // until it says otherwise: the ramp holds, and its
                        // offers are withdrawn.
                        if alive.fetch_sub(1, Ordering::AcqRel) == 1 {
                            steering.guest_demand(true, false);
                            steering.guest_offers(0, true);
                        }
                    });
                if spawned.is_err() {
                    live.fetch_sub(1, Ordering::AcqRel);
                }
            }
        })
        .map(|_| ())
        .map_err(|e| format!("cannot start the guest memory listener: {e}"))
}

/// The steering rule, as arithmetic: a step up while the host compresses,
/// a step down once it has been quiet for a few seconds, and
/// nothing in between. Quiet, not free: an 8 GB Mac shows a few hundred
/// megabytes free at the best of times, and a deflate that waited for more
/// left the balloon inflated through the install after the big one, which
/// then paid for the cache it did not have.
const QUIET_POLLS_BEFORE_DEFLATE: u32 = 5;

fn as_pages(bytes: u64) -> u32 {
    (bytes / BALLOON_PAGE_SIZE).min(u64::from(u32::MAX)) as u32
}

/// How much of the guest the ramp may reach at a level.
/// How hard the guest's warm loop may push (`guest/agent/src/warm.rs`): a
/// multiple of the sliver of cache it asks its kernel for every six seconds.
/// The loop's own limit is the guest's measured stall, so this is need and
/// not permission: 1 on a Mac with memory to spare, 8 while it compresses,
/// swaps, or is at Warn or was a minute ago (the ramp's own tests, with the
/// hysteresis they already carry), 20, the loop's ceiling of a hundredth of
/// the cache a period, at Critical. A gain that is wrong costs the guest
/// minutes of convergence and never a stalled container.
const WARM_GAIN_PRESSED: u32 = 8;
const WARM_GAIN_CRITICAL: u32 = 20;
/// Said again this often: the agent forgets a gain nobody repeats, so a
/// host that went away is not a host under pressure for ever.
const WARM_GAIN_REFRESH: Duration = Duration::from_secs(60);

/// `level` is what the Mac reports; `pressed` is the ramp's wider test (a
/// level, one within the last minute, or an overcommitted host);
/// `compressing` is the compressor having moved within the ramp's grace.
fn warm_gain(level: u32, pressed: bool, compressing: bool) -> u32 {
    if level == Pressure::Critical as u32 {
        WARM_GAIN_CRITICAL
    } else if pressed || compressing {
        WARM_GAIN_PRESSED
    } else {
        1
    }
}

/// `LIGHTER_RECLAIM_AHEAD=0` leaves the balloon to take its step with no
/// reclaim asked of the guest first, which is what every 0.7.2 machine did
/// whatever this file said (`ask_agent`), and so is the arm to measure the
/// request against now that it arrives.
fn reclaim_ahead_enabled() -> bool {
    std::env::var("LIGHTER_RECLAIM_AHEAD")
        .ok()
        .is_none_or(|v| v != "0")
}

/// `LIGHTER_WARM_GAIN=<n>` says that gain whatever the Mac's pressure is, so
/// the warm case can be measured at a known one: the benchmark host's own
/// pressure is weather.
fn warm_gain_forced() -> Option<u32> {
    std::env::var("LIGHTER_WARM_GAIN").ok()?.parse().ok()
}

fn cap_pages(level: u32, total_bytes: u64) -> u32 {
    let fraction = match level {
        level if level == Pressure::Warn as u32 => WARN_FRACTION,
        level if level == Pressure::Critical as u32 => CRITICAL_FRACTION,
        _ => STEER_CAP_FRACTION,
    };
    as_pages(total_bytes / fraction)
}

/// The ramp's rule, as arithmetic. Up while the host is under pressure or
/// compressing, by the level's step, to the level's cap; down by the small
/// step once the host has been quiet for a few seconds with no pressure;
/// nothing in between. A level that drops leaves the ramp where it is, above
/// the new cap, to ease down: a target that fell to nothing had the guest
/// refill what the host then compressed again. Never past the guest's
/// reserve.
fn steer(pages: u32, compressed: u64, quiet_for: u32, total_bytes: u64, level: u32) -> u32 {
    let normal = level == Pressure::Normal as u32;
    let cap = cap_pages(level, total_bytes);
    let reserve = as_pages(total_bytes - total_bytes / GUEST_RESERVE_FRACTION);
    let small = as_pages((total_bytes / STEP_FRACTION).max(STEP_MIN_BYTES));
    let step_up = match level {
        level if level == Pressure::Warn as u32 => as_pages(total_bytes / WARN_STEP_FRACTION),
        level if level == Pressure::Critical as u32 => {
            as_pages(total_bytes / CRITICAL_STEP_FRACTION)
        }
        _ => small,
    };
    let pages = pages.min(reserve);
    if !normal || compressed >= COMPRESSING_BYTES_PER_POLL {
        if pages >= cap {
            pages
        } else {
            pages.saturating_add(step_up).min(cap)
        }
    } else if quiet_for >= QUIET_POLLS_BEFORE_DEFLATE {
        pages.saturating_sub(small)
    } else {
        pages
    }
}

/// The pressure-level half of the policy: the cap and step of the ramp.
struct Levels(Arc<Steering>);

impl Observer for Levels {
    fn pressure(&self, level: Pressure) {
        let steering = &self.0;
        let previous = steering.level.swap(level as u32, Ordering::Relaxed);
        if previous != level as u32 {
            tracing::info!(
                pressure = ?level,
                cap_mib = (u64::from(cap_pages(level as u32, steering.total_bytes())) * BALLOON_PAGE_SIZE) >> 20,
                "host memory pressure changed"
            );
            let mut state = steering
                .compression
                .lock()
                .expect("compression policy poisoned");
            state.quiet_polls = 0;
            if level != Pressure::Normal {
                state.pressure_until = Some(Instant::now() + PRESSURE_MEMORY);
            }
        }
        steering.apply();
    }
}

/// What the host has compressed, read the way `vm_stat` reads it.
struct HostMemory {
    page: u64,
    ram: u64,
}

#[repr(C)]
#[derive(Default)]
struct VmStatistics64 {
    free_count: u32,
    active_count: u32,
    inactive_count: u32,
    wire_count: u32,
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: u32,
    speculative_count: u32,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor_page_count: u32,
    throttled_count: u32,
    external_page_count: u32,
    internal_page_count: u32,
    total_uncompressed_pages_in_compressor: u64,
}

const HOST_VM_INFO64: i32 = 4;

unsafe extern "C" {
    fn mach_host_self() -> u32;
    fn host_statistics64(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
}

/// `struct xsw_usage`, as `sysctl vm.swapusage` reads it.
#[repr(C)]
#[derive(Default)]
struct XswUsage {
    total: u64,
    avail: u64,
    used: u64,
    pagesize: u32,
    encrypted: bool,
}

fn sysctl_u64(name: &std::ffi::CStr) -> Option<u64> {
    let mut value: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: an output of the size given, for a name that yields a u64.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut value as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(value)
}

/// Swap in use and swap in total, in bytes; zeros when the Mac will not say.
fn swap_usage() -> (u64, u64) {
    let mut usage = XswUsage::default();
    let mut len = std::mem::size_of::<XswUsage>();
    // SAFETY: the struct is `struct xsw_usage` field for field, and the
    // length says so.
    let rc = unsafe {
        libc::sysctlbyname(
            c"vm.swapusage".as_ptr(),
            (&mut usage as *mut XswUsage).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 {
        (usage.used, usage.total)
    } else {
        (0, 0)
    }
}

impl HostMemory {
    fn new() -> HostMemory {
        // SAFETY: a plain query with no pointers of ours involved.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
        HostMemory {
            page,
            ram: sysctl_u64(c"hw.memsize").unwrap_or(0),
        }
    }

    fn sample(&self) -> Option<HostSample> {
        let mut stats = VmStatistics64::default();
        let mut count = (std::mem::size_of::<VmStatistics64>() / 4) as u32;
        // SAFETY: the struct matches `struct vm_statistics64` field for field
        // (38 integers), and `count` says so.
        let rc = unsafe {
            host_statistics64(
                mach_host_self(),
                HOST_VM_INFO64,
                (&mut stats as *mut VmStatistics64).cast(),
                &mut count,
            )
        };
        if rc != 0 {
            return None;
        }
        let (swap_used, swap_total) = swap_usage();
        Some(HostSample {
            compressed: stats.compressions * self.page,
            free: u64::from(stats.free_count) * self.page,
            compressor: u64::from(stats.compressor_page_count) * self.page,
            swap_used,
            swap_total,
            ram: self.ram,
        })
    }
}

/// One reading: how much the host has compressed since boot, what it has
/// free, what its compressor holds, and its swap.
#[derive(Clone, Copy)]
struct HostSample {
    compressed: u64,
    free: u64,
    compressor: u64,
    swap_used: u64,
    swap_total: u64,
    ram: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const NORMAL: u32 = Pressure::Normal as u32;
    const WARN: u32 = Pressure::Warn as u32;
    const CRITICAL: u32 = Pressure::Critical as u32;

    fn mib(pages: u32) -> u64 {
        (u64::from(pages) * BALLOON_PAGE_SIZE) >> 20
    }

    /// The three levels have to be ordered, because a policy that treats
    /// `Warn` and `Critical` the same either overreacts to a browser tab or
    /// underreacts to the machine swapping.
    #[test]
    fn the_levels_are_ordered_and_distinct() {
        assert!(Pressure::Normal < Pressure::Warn);
        assert!(Pressure::Warn < Pressure::Critical);
        const { assert!(WARN_FRACTION > CRITICAL_FRACTION) };
        const { assert!(WARN_STEP_FRACTION > CRITICAL_STEP_FRACTION) };
    }

    /// The arithmetic, without a hypervisor: an 8 GiB guest, and how far
    /// each level lets the ramp go.
    #[test]
    fn each_level_caps_the_ramp_at_a_sensible_share() {
        let ram: u64 = 8 << 30;
        assert_eq!(mib(cap_pages(NORMAL, ram)), 1024);
        assert_eq!(mib(cap_pages(WARN, ram)), 2048);
        assert_eq!(mib(cap_pages(CRITICAL, ram)), 4096);
        // And the page count has to fit the field the guest reads it from.
        assert!(u64::from(cap_pages(CRITICAL, 1 << 40)) <= u64::from(u32::MAX));
    }

    /// With no host pressure: a step up while compressing, a step down
    /// once quiet for long enough, and a hold in between; never past the
    /// cap or below zero.
    #[test]
    fn steering_steps_up_while_compressing_and_eases_when_quiet() {
        let total = 4 << 30;
        let cap = cap_pages(NORMAL, total);
        let up = steer(0, 64 << 20, 0, total, NORMAL);
        assert_eq!(mib(up), 32);
        assert_eq!(
            steer(up, 0, 2, total, NORMAL),
            up,
            "quiet but not for long: held"
        );
        assert_eq!(
            steer(up, 1 << 20, 0, total, NORMAL),
            up,
            "a trickle, so no quiet yet: held"
        );
        let down = steer(up, 0, QUIET_POLLS_BEFORE_DEFLATE, total, NORMAL);
        assert_eq!(mib(up - down), 32);
        assert_eq!(
            steer(cap, 1 << 30, 0, total, NORMAL),
            cap,
            "never past the cap"
        );
        assert_eq!(steer(1, 0, 30, total, NORMAL), 0, "never below zero");
    }

    #[test]
    fn large_guests_reach_the_cap_within_32_polls_and_recover() {
        for gib in [8, 12, 24, 48, 128] {
            let total = gib << 30;
            let mut pages = 0;
            for _ in 0..32 {
                pages = steer(pages, 64 << 20, 0, total, NORMAL);
            }
            assert_eq!(u64::from(pages) * BALLOON_PAGE_SIZE, total / 8);
            for _ in 0..32 {
                pages = steer(pages, 0, QUIET_POLLS_BEFORE_DEFLATE, total, NORMAL);
            }
            assert_eq!(pages, 0);
        }
        assert_eq!(mib(steer(0, 64 << 20, 0, 24 << 30, NORMAL)), 96);
    }

    /// A level raises the cap and the step: a quarter of the guest within
    /// eight polls at Warn, half at Critical, with or without compression.
    #[test]
    fn a_level_raises_the_cap_and_the_ramp_climbs_it_in_eight_polls() {
        let total: u64 = 16 << 30;
        for (level, share) in [(WARN, 4), (CRITICAL, 2)] {
            let mut pages = 0;
            let mut polls = 0;
            while u64::from(pages) * BALLOON_PAGE_SIZE < total / share {
                pages = steer(pages, 0, 0, total, level);
                polls += 1;
                assert!(polls <= 8, "reached in eight polls, not {polls}");
            }
            assert_eq!(u64::from(pages) * BALLOON_PAGE_SIZE, total / share);
            assert_eq!(steer(pages, 0, 0, total, level), pages, "and no further");
            assert_eq!(
                steer(pages, 0, 30, total, level),
                pages,
                "a quiet host under pressure does not ease"
            );
        }
    }

    /// A level that drops leaves the ramp above the new cap, to ease down by
    /// the small step only once the host has been quiet: a target that fell
    /// to nothing on Normal had the guest refill what the host then
    /// compressed again.
    #[test]
    fn a_level_drop_is_a_plateau_that_eases_when_quiet() {
        let total: u64 = 16 << 30;
        let mut pages = 0;
        for _ in 0..8 {
            pages = steer(pages, 0, 0, total, WARN);
        }
        assert_eq!(mib(pages), 4096);
        let held = steer(pages, 0, 0, total, NORMAL);
        assert_eq!(held, pages, "Normal, not quiet yet: nothing moves");
        assert_eq!(
            steer(pages, 64 << 20, 0, total, NORMAL),
            pages,
            "compressing above the Normal cap: no step up, no step down"
        );
        let eased = steer(pages, 0, QUIET_POLLS_BEFORE_DEFLATE, total, NORMAL);
        assert_eq!(mib(pages - eased), 64, "a 256th of 16 GiB");
    }

    #[test]
    fn the_warm_gain_follows_the_hosts_need() {
        let (warn, critical) = (Pressure::Warn as u32, Pressure::Critical as u32);
        assert_eq!(warm_gain(NORMAL, false, false), 1);
        // Compressing, or a Warn remembered, at a level that reads Normal.
        assert_eq!(warm_gain(NORMAL, false, true), WARM_GAIN_PRESSED);
        assert_eq!(warm_gain(NORMAL, true, false), WARM_GAIN_PRESSED);
        assert_eq!(warm_gain(warn, true, true), WARM_GAIN_PRESSED);
        assert_eq!(warm_gain(critical, true, true), WARM_GAIN_CRITICAL);
    }

    fn steering_for_tests(ram_bytes: u64) -> (Steering, Arc<Mutex<VirtioMmio>>) {
        use crate::irq::NullIrq;
        use crate::memory::GuestMemory;
        use crate::virtio::balloon::Balloon;

        let balloon = Arc::new(BalloonState::default());
        let transport = Arc::new(Mutex::new(VirtioMmio::new(
            Box::new(Balloon::new(balloon.clone())),
            Arc::new(GuestMemory::detached()),
            Arc::new(NullIrq),
        )));
        let steering = Steering {
            balloon,
            transport: transport.clone(),
            ram_bytes,
            vsock: None,
            reclaiming: Arc::new(AtomicBool::new(false)),
            warm_gain: Arc::new(Mutex::new(None)),
            saying_gain: Arc::new(AtomicBool::new(false)),
            level: AtomicU32::new(NORMAL),
            compression: Mutex::new(CompressionState::default()),
            apply_lock: Mutex::new(()),
            guest_pages: AtomicU32::new(0),
        };
        (steering, transport)
    }

    fn target_mib(transport: &Arc<Mutex<VirtioMmio>>) -> u64 {
        use crate::bus::MmioDevice;
        let mut config = [0; 4];
        transport.lock().unwrap().read(0x100, &mut config);
        (u64::from(u32::from_le_bytes(config)) * BALLOON_PAGE_SIZE) >> 20
    }

    /// What `Levels::pressure` does, for a `Steering` held by value.
    fn pressure(steering: &Steering, level: Pressure) {
        if steering.level.swap(level as u32, Ordering::Relaxed) != level as u32 {
            let mut state = steering.compression.lock().unwrap();
            state.quiet_polls = 0;
            if level != Pressure::Normal {
                state.pressure_until = Some(Instant::now() + PRESSURE_MEMORY);
            }
        }
        steering.apply();
    }

    /// A guest that has said it is fine, and long enough ago: the ramp's
    /// precondition.
    fn healthy(steering: &Steering) {
        steering.guest_demand(false, false);
        steering.compression.lock().unwrap().healthy_since = Some(Instant::now() - HOLD_PATIENCE);
        steering.apply();
    }

    /// Polls with no compression: a level moves the ramp up, and quiet
    /// brings it down.
    fn polls(steering: &Steering, n: usize) {
        for _ in 0..n {
            steering.steer(0);
        }
    }

    /// A level change reaches the device on the next poll, and a guest that
    /// shrank between polls sees its target clamped without a new pressure
    /// event.
    #[test]
    fn the_ramp_reaches_the_device() {
        use crate::bus::MmioDevice;

        let (steering, transport) = steering_for_tests(24 << 30);
        healthy(&steering);
        pressure(&steering, Pressure::Warn);
        assert_eq!(
            target_mib(&transport),
            0,
            "a level sets no target of its own"
        );
        polls(&steering, 8);
        assert_eq!(target_mib(&transport), 6144);
        let mut generation = [0; 4];
        transport.lock().unwrap().read(0xfc, &mut generation);
        assert!(
            u32::from_le_bytes(generation) >= 8,
            "each change notified the guest"
        );
    }

    /// A level that drops restarts the ramp's patience: five quiet polls of
    /// grace before it eases, even if the host had been quiet through the
    /// pressure, so a host flapping between Warn and Normal does not have
    /// the balloon eased on every Normal second.
    #[test]
    fn a_level_drop_restarts_the_patience_before_easing() {
        let (steering, transport) = steering_for_tests(16 << 30);
        healthy(&steering);
        pressure(&steering, Pressure::Warn);
        polls(&steering, 12);
        assert_eq!(
            target_mib(&transport),
            4096,
            "quiet polls under Warn do not ease"
        );
        pressure(&steering, Pressure::Normal);
        polls(&steering, QUIET_POLLS_BEFORE_DEFLATE as usize + 3);
        assert_eq!(
            target_mib(&transport),
            4096,
            "a Normal within a minute of the Warn is the level bouncing: steered as at Warn"
        );
        steering.compression.lock().unwrap().pressure_until = None;
        polls(&steering, QUIET_POLLS_BEFORE_DEFLATE as usize - 1);
        assert_eq!(target_mib(&transport), 4096, "grace");
        polls(&steering, 1);
        assert_eq!(target_mib(&transport), 4096 - 64, "then a 256th a second");
    }

    /// A release the moment the level reads Normal after a Warn is what
    /// handed 4 GiB back in one step on 2026-09-20: answered as under
    /// pressure, ignored, for as long as the Warn is remembered.
    #[test]
    fn a_release_within_a_minute_of_a_warn_is_ignored() {
        let (steering, transport) = steering_for_tests(16 << 30);
        healthy(&steering);
        pressure(&steering, Pressure::Warn);
        polls(&steering, 8);
        assert_eq!(target_mib(&transport), 4096);
        steering
            .balloon
            .set_actual_pages_for_test(as_pages(4 << 30));
        pressure(&steering, Pressure::Normal);
        steering.guest_demand(false, true);
        polls(&steering, 3);
        assert_eq!(target_mib(&transport), 4096, "the release is ignored");
        assert!(!steering.compression.lock().unwrap().releasing);
    }

    /// With no pressure by any sign, a release comes down the ramp a 32nd
    /// of RAM a second: 4 GiB in eight polls, never in one.
    #[test]
    fn a_release_with_no_pressure_comes_down_the_ramp() {
        let (steering, transport) = steering_for_tests(16 << 30);
        healthy(&steering);
        pressure(&steering, Pressure::Warn);
        polls(&steering, 8);
        steering
            .balloon
            .set_actual_pages_for_test(as_pages(4 << 30));
        pressure(&steering, Pressure::Normal);
        steering.compression.lock().unwrap().pressure_until = None;
        steering.guest_demand(false, true);
        let mut last = target_mib(&transport);
        assert_eq!(last, 4096, "from the next poll");
        for expected in [3584, 3072, 2560, 2048, 1536, 1024, 512, 0] {
            polls(&steering, 1);
            let now = target_mib(&transport);
            assert!(last - now <= 512, "never more than a step: {last} -> {now}");
            assert_eq!(now, expected);
            last = now;
        }
    }

    /// A guest whose every task is stalled on memory comes down two steps a
    /// poll: 4 GiB in four polls, not eight.
    #[test]
    fn a_stalled_guest_comes_down_twice_as_fast() {
        let (steering, transport) = steering_for_tests(16 << 30);
        healthy(&steering);
        pressure(&steering, Pressure::Warn);
        polls(&steering, 8);
        steering
            .balloon
            .set_actual_pages_for_test(as_pages(4 << 30));
        steering.guest_stall(4500, 1200);
        steering.guest_demand(true, false);
        assert_eq!(target_mib(&transport), 4096, "from the next poll");
        for expected in [3072, 2048, 1024, 0] {
            polls(&steering, 1);
            assert_eq!(
                target_mib(&transport),
                expected,
                "two steps a poll while every task stalls"
            );
        }
        // With no full stall the pace is the usual one.
        let (steering, transport) = steering_for_tests(16 << 30);
        healthy(&steering);
        pressure(&steering, Pressure::Warn);
        polls(&steering, 8);
        steering
            .balloon
            .set_actual_pages_for_test(as_pages(4 << 30));
        steering.guest_stall(4500, 300);
        steering.guest_demand(true, false);
        polls(&steering, 1);
        assert_eq!(
            target_mib(&transport),
            3584,
            "one step: some tasks stall, not all"
        );
    }

    /// Each step down is no more than half of what the host has free: a
    /// guest is never handed memory the Mac would have to page for.
    #[test]
    fn deflation_is_paced_by_what_the_host_has_free() {
        let (steering, transport) = steering_for_tests(16 << 30);
        healthy(&steering);
        pressure(&steering, Pressure::Warn);
        polls(&steering, 8);
        steering
            .balloon
            .set_actual_pages_for_test(as_pages(4 << 30));
        pressure(&steering, Pressure::Normal);
        steering.compression.lock().unwrap().pressure_until = None;
        steering.observe(&HostSample {
            compressed: 0,
            free: 256 << 20,
            compressor: 0,
            swap_used: 0,
            swap_total: 0,
            ram: 48 << 30,
        });
        steering.guest_demand(false, true);
        polls(&steering, 1);
        assert_eq!(
            target_mib(&transport),
            4096 - 128,
            "half of 256 MiB free, not the ramp's 512"
        );
        steering
            .balloon
            .set_actual_pages_for_test(as_pages((4096 - 128) << 20));
        polls(&steering, 1);
        assert_eq!(
            target_mib(&transport),
            4096 - 256,
            "and again as the driver follows"
        );
    }

    /// A Mac reporting Normal with a quarter of its RAM in the compressor,
    /// or half its swap in use, is under pressure: the ramp climbs the
    /// Warn cap at the Warn pace.
    #[test]
    fn an_overcommitted_host_is_pressure_whatever_it_reports() {
        let (steering, transport) = steering_for_tests(16 << 30);
        healthy(&steering);
        steering.observe(&HostSample {
            compressed: 0,
            free: 1 << 30,
            compressor: 13 << 30,
            swap_used: 0,
            swap_total: 0,
            ram: 48 << 30,
        });
        polls(&steering, 8);
        assert_eq!(target_mib(&transport), 4096, "the Warn cap, at Normal");
        steering.observe(&HostSample {
            compressed: 0,
            free: 1 << 30,
            compressor: 0,
            swap_used: 7 << 30,
            swap_total: 9 << 30,
            ram: 48 << 30,
        });
        steering.guest_demand(false, true);
        polls(&steering, 2);
        assert_eq!(
            target_mib(&transport),
            4096,
            "swapping: a release is ignored too"
        );
        assert!(steering.compression.lock().unwrap().overcommitted);
        // Four of a five-gigabyte swap file is 80% of the file and a twelfth
        // of the Mac's RAM, under the line to leave (an eighth less a fifth):
        // not overcommitted. The file's size says nothing.
        steering.observe(&HostSample {
            compressed: 0,
            free: 1 << 30,
            compressor: 0,
            swap_used: 4 << 30,
            swap_total: 5 << 30,
            ram: 48 << 30,
        });
        assert!(!steering.compression.lock().unwrap().overcommitted);
        // Hysteresis: in at a quarter of RAM compressed, out only a fifth
        // below it, so a Mac sitting on the line does not flip each poll.
        let sample = |compressor: u64| HostSample {
            compressed: 0,
            free: 1 << 30,
            compressor,
            swap_used: 0,
            swap_total: 0,
            ram: 8 << 30,
        };
        steering.observe(&sample((2 << 30) + (1 << 20)));
        assert!(
            steering.compression.lock().unwrap().overcommitted,
            "over a quarter"
        );
        steering.observe(&sample((2 << 30) - (100 << 20)));
        assert!(
            steering.compression.lock().unwrap().overcommitted,
            "just under: still in"
        );
        steering.observe(&sample((2 << 30) - (500 << 20)));
        assert!(
            !steering.compression.lock().unwrap().overcommitted,
            "a fifth below: out"
        );
    }

    /// `need` freezes the ramp where the balloon is and brings it down the
    /// paced steps from there, at any level; it follows deflate-on-OOM down
    /// too. Recovery moves again only after the guest has been fine for a
    /// while, from where it froze.
    #[test]
    fn guest_need_brings_the_ramp_down_from_where_the_balloon_is() {
        let (steering, transport) = steering_for_tests(8 << 30);
        healthy(&steering);
        for _ in 0..32 {
            steering.steer(64 << 20);
        }
        assert_eq!(target_mib(&transport), 1024, "the Normal cap, compressing");
        steering
            .balloon
            .set_actual_pages_for_test(as_pages(1 << 30));
        steering.guest_demand(true, false);
        assert_eq!(
            target_mib(&transport),
            1024,
            "no host pressure: the ramp comes down from the next poll, not at once"
        );
        for expected in [768, 512, 256, 0, 0, 0, 0, 0] {
            steering.steer(u64::MAX);
            assert_eq!(
                target_mib(&transport),
                expected,
                "a 32nd of RAM a poll, and compression must not refill it while short"
            );
        }
        steering.balloon.set_actual_pages_for_test(0);
        // Fine again, and long enough: the ramp climbs the Warn cap.
        healthy(&steering);
        pressure(&steering, Pressure::Warn);
        polls(&steering, 8);
        assert_eq!(target_mib(&transport), 2048);
        // The driver got half of it, then the guest says it is short: the
        // ramp freezes at what the balloon has and comes down from there,
        // under Warn all the same. A hold alone starved the M5's guest
        // under a host that read as overcommitted (2026-09-20 19:16Z).
        steering
            .balloon
            .set_actual_pages_for_test(as_pages(1 << 30));
        steering.guest_demand(true, false);
        assert_eq!(
            target_mib(&transport),
            1024,
            "frozen where the balloon is, from here down"
        );
        polls(&steering, 1);
        assert_eq!(target_mib(&transport), 768, "a step down under Warn");
        polls(&steering, 1);
        assert_eq!(target_mib(&transport), 512);
        steering
            .balloon
            .set_actual_pages_for_test(as_pages(256 << 20));
        polls(&steering, 1);
        assert_eq!(
            target_mib(&transport),
            256,
            "follows deflate-on-OOM down, never up"
        );
        // Recovery: not on the fine line itself, five seconds after it.
        steering.guest_demand(false, false);
        polls(&steering, 3);
        assert_eq!(target_mib(&transport), 256, "still held");
        steering.compression.lock().unwrap().healthy_since = Some(Instant::now() - HOLD_PATIENCE);
        polls(&steering, 1);
        assert_eq!(
            target_mib(&transport),
            256 + 256,
            "one Warn step from where it froze"
        );
    }

    /// `release` is the guest asking for its reserve back: withdrawn and
    /// held with no host pressure, as ever; ignored under pressure, where
    /// inflation itself keeps free memory low while the guest still has
    /// cache to give.
    #[test]
    fn guest_release_withdraws_at_normal_and_is_ignored_under_pressure() {
        let (steering, transport) = steering_for_tests(8 << 30);
        healthy(&steering);
        for _ in 0..32 {
            steering.steer(64 << 20);
        }
        assert_eq!(target_mib(&transport), 1024);
        steering
            .balloon
            .set_actual_pages_for_test(as_pages(1 << 30));
        steering.guest_demand(false, true);
        assert_eq!(target_mib(&transport), 1024, "released from the next poll");
        steering.steer(u64::MAX);
        assert_eq!(
            target_mib(&transport),
            768,
            "a step down, compression or not"
        );
        polls(&steering, 3);
        assert_eq!(target_mib(&transport), 0, "and gone in four");
        steering.balloon.set_actual_pages_for_test(0);
        healthy(&steering);
        pressure(&steering, Pressure::Warn);
        polls(&steering, 2);
        assert_eq!(target_mib(&transport), 512);
        steering.guest_demand(false, true);
        polls(&steering, 1);
        assert_eq!(
            target_mib(&transport),
            768,
            "a release under Warn does not stop the ramp"
        );
    }

    /// Before the guest has spoken at all, the ramp waits: the agent is not
    /// up yet, and a guest that never says how it is gets nothing taken by
    /// force. A fine line, five seconds, and the ramp climbs.
    #[test]
    fn the_ramp_waits_for_the_guest_to_say_it_is_fine() {
        let (steering, transport) = steering_for_tests(8 << 30);
        pressure(&steering, Pressure::Critical);
        polls(&steering, 8);
        assert_eq!(target_mib(&transport), 0);
        steering.guest_demand(false, false);
        polls(&steering, 2);
        assert_eq!(target_mib(&transport), 0, "fine, but not for long enough");
        healthy(&steering);
        polls(&steering, 8);
        assert_eq!(target_mib(&transport), 4096);
    }

    /// An inflation that makes no progress for three seconds is one the
    /// guest cannot make, whatever its last line said: its agent may be
    /// alive and not scheduled, which is what starvation looks like from
    /// here. The ramp is held where the balloon is until the guest says it
    /// is fine again.
    #[test]
    fn a_stalled_inflation_holds_the_ramp() {
        let (steering, transport) = steering_for_tests(8 << 30);
        healthy(&steering);
        pressure(&steering, Pressure::Warn);
        polls(&steering, 8);
        assert_eq!(target_mib(&transport), 2048);
        let t = Instant::now();
        steering.inflation_stalled(t);
        steering.inflation_stalled(t + INFLATION_STALL - Duration::from_millis(1));
        assert_eq!(target_mib(&transport), 2048, "not stalled yet");
        // Progress resets the clock.
        steering
            .balloon
            .set_actual_pages_for_test(as_pages(512 << 20));
        steering.inflation_stalled(t + INFLATION_STALL);
        steering.inflation_stalled(t + 2 * INFLATION_STALL - Duration::from_millis(1));
        assert_eq!(target_mib(&transport), 2048, "it grew; still inflating");
        steering.inflation_stalled(t + 2 * INFLATION_STALL);
        assert_eq!(target_mib(&transport), 512, "stalled: held at what it has");
        // Held, the target is met, and nothing is withdrawn twice.
        steering.inflation_stalled(t + Duration::from_secs(60));
        polls(&steering, 3);
        assert_eq!(target_mib(&transport), 512);
        healthy(&steering);
        polls(&steering, 1);
        assert_eq!(
            target_mib(&transport),
            512 + 256,
            "fine again: one step from where it froze"
        );
    }

    /// The driver's retries each reclaim a little, and may get a page or
    /// two: a balloon creeping up by a megabyte a second is stalled, not
    /// inflating.
    #[test]
    fn a_creeping_inflation_is_a_stalled_one() {
        let (steering, transport) = steering_for_tests(8 << 30);
        healthy(&steering);
        pressure(&steering, Pressure::Warn);
        polls(&steering, 8);
        let t = Instant::now();
        for second in 0..3 {
            steering
                .balloon
                .set_actual_pages_for_test(as_pages(second << 20));
            steering.inflation_stalled(t + Duration::from_secs(second));
            assert_eq!(
                target_mib(&transport),
                2048,
                "creeping, not yet three seconds"
            );
        }
        steering
            .balloon
            .set_actual_pages_for_test(as_pages(3 << 20));
        steering.inflation_stalled(t + Duration::from_secs(3));
        assert_eq!(
            target_mib(&transport),
            3,
            "stalled: held at the three megabytes it has"
        );
    }

    /// The hold never raises a target: a balloon holding more than the ramp
    /// asked (the guest's own offers) leaves the ramp where it was.
    #[test]
    fn a_held_ramp_is_never_more_than_the_ramp() {
        let (steering, transport) = steering_for_tests(8 << 30);
        healthy(&steering);
        pressure(&steering, Pressure::Warn);
        polls(&steering, 2);
        assert_eq!(target_mib(&transport), 512);
        steering
            .balloon
            .set_actual_pages_for_test(as_pages(3 << 30));
        steering.guest_demand(true, false);
        assert_eq!(
            target_mib(&transport),
            512,
            "the ramp, not the larger holding"
        );
    }

    /// The statistics struct is the kernel's, integer for integer.
    #[test]
    fn the_statistics_struct_is_the_kernels_size() {
        assert_eq!(std::mem::size_of::<VmStatistics64>() / 4, 38);
        assert!(HostMemory::new().sample().is_some());
    }
}
