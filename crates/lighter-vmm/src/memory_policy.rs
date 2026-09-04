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
//! So the policy is short: watch what the system watches, and translate.
//! macOS's own three pressure levels are the input, because reacting to the
//! same signal the kernel reacts to means reacting at the same moment — rather
//! than on a timer, or on a guess about what "low memory" means on a machine
//! whose size we do not know.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::mempressure::{Observer, Pressure, Watcher};
use crate::virtio::balloon::{BALLOON_PAGE_SIZE, BalloonState};
use crate::virtio::mmio::VirtioMmio;

/// What fraction of guest RAM to reclaim at each level.
///
/// Deliberately not aggressive at `Warn`: the system reclaiming is normal on a
/// Mac and happens long before anything is in trouble, so taking half the
/// guest's memory then would make every browser tab a stutter in the
/// container. `Critical` means macOS is about to swap or kill something, and at
/// that point a slow guest is much better than a wedged host.
const WARN_FRACTION: u64 = 4; // a quarter
const CRITICAL_FRACTION: u64 = 2; // a half

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
/// the host is compressing and eases a smaller step each second once it
/// has stopped and has free memory again: prompt on the way up, gradual on
/// the way down, so the guest's cache cannot refill the host into the same
/// corner every other second. The pressure level keeps its say as a
/// minimum. On a 48 GB Mac the compressor never moves and the target
/// stays at zero.
const COMPRESSING_BYTES_PER_POLL: u64 = 64 << 20;
const INFLATE_STEP_BYTES: u64 = 32 << 20;
const DEFLATE_STEP_BYTES: u64 = 32 << 20;
/// The steering never asks for more than this share of guest RAM; the
/// pressure levels may.
const STEER_CAP_FRACTION: u64 = 8;
const POLL: Duration = Duration::from_secs(1);

/// The guest port the agent's control channel listens on.
pub const AGENT_CONTROL_PORT: u32 = 2376;
/// The host port the agent dials to say what it can spare (`memory_guest`).
pub const MEMORY_PORT: u32 = 2381;
/// The guest keeps at least this fraction of its RAM out of the balloon.
const GUEST_RESERVE_FRACTION: u64 = 16;

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
            level: AtomicU32::new(Pressure::Normal as u32),
            level_pages: AtomicU32::new(0),
            steer_pages: AtomicU32::new(0),
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
                .steer_pages
                .store(((mib << 20) / BALLOON_PAGE_SIZE) as u32, Ordering::Relaxed);
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
                    let mut quiet_for = 0u32;
                    // `LIGHTER_MEM_TRACE=1`: what the guest has reported free
                    // and what the host holds, every second, to the log.
                    let trace = std::env::var("LIGHTER_MEM_TRACE").map(|v| v == "1").unwrap_or(false);
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(POLL);
                        if trace {
                            let (resident, internal, reusable) = crate::footprint::split();
                            eprintln!(
                                "MEMTRACE footprint_mib={} resident_mib={} internal_mib={} reusable_mib={} reported_mib={} offered_mib={} steer_mib={} level_mib={}",
                                crate::footprint::bytes() >> 20,
                                resident >> 20,
                                internal >> 20,
                                reusable >> 20,
                                steering.balloon.reported_bytes() >> 20,
                                steering.balloon.offered_bytes() >> 20,
                                (steering.steer_pages.load(Ordering::Relaxed) as u64 * BALLOON_PAGE_SIZE) >> 20,
                                (steering.level_pages.load(Ordering::Relaxed) as u64 * BALLOON_PAGE_SIZE) >> 20
                            );
                        }
                        let Some(now) = host.sample() else { continue };
                        if let Some(then) = last {
                            let compressed = now.compressed.saturating_sub(then.compressed);
                            quiet_for = if compressed == 0 { quiet_for + 1 } else { 0 };
                            steering.steer(compressed, quiet_for);
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

struct Steering {
    balloon: Arc<BalloonState>,
    transport: Arc<Mutex<VirtioMmio>>,
    ram_bytes: u64,
    level: AtomicU32,
    /// What the pressure level asks for, what the compressor asks for, and
    /// what the guest itself offers: the balloon's target is the largest.
    level_pages: AtomicU32,
    steer_pages: AtomicU32,
    guest_pages: AtomicU32,
}

impl Steering {
    fn cap_pages(&self) -> u32 {
        (self.ram_bytes / STEER_CAP_FRACTION / BALLOON_PAGE_SIZE).min(u64::from(u32::MAX)) as u32
    }

    fn steer(&self, compressed: u64, quiet_for: u32) {
        let before = self.steer_pages.load(Ordering::Relaxed);
        let after = steer(before, compressed, quiet_for, self.cap_pages());
        if after != before {
            self.steer_pages.store(after, Ordering::Relaxed);
            self.apply();
        }
    }

    /// The guest says it can spare `spare_mib` beyond what the balloon
    /// already holds; `release` asks the whole balloon back. An offer under
    /// 64 MiB holds the target where it is. A step is at most 2 GiB, so
    /// the next second's offer is measured against inflation that has
    /// actually happened rather than a target still being filled — the
    /// balloon's own count of what it holds lags its allocations, and a
    /// target summed from the two overshoots the guest's reserve.
    fn guest_offers(&self, spare_mib: u64, release: bool) {
        const STEP_MIB: u64 = 2048;
        let cap = ((self.ram_bytes - self.ram_bytes / GUEST_RESERVE_FRACTION) / BALLOON_PAGE_SIZE)
            .min(u64::from(u32::MAX)) as u32;
        let pages = if release {
            0
        } else if spare_mib < 64 {
            return;
        } else {
            let actual = u64::from(self.balloon.actual_pages());
            (actual + (spare_mib.min(STEP_MIB) << 20) / BALLOON_PAGE_SIZE)
                .min(u64::from(cap))
                .min(u64::from(u32::MAX)) as u32
        };
        if pages != self.guest_pages.swap(pages, Ordering::Relaxed) {
            self.apply();
        }
    }

    fn apply(&self) {
        let pages = self
            .level_pages
            .load(Ordering::Relaxed)
            .max(self.steer_pages.load(Ordering::Relaxed))
            .max(self.guest_pages.load(Ordering::Relaxed));
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
fn memory_guest(
    vsock: Arc<crate::virtio::vsock::VsockShared>,
    steering: Arc<Steering>,
) -> Result<(), String> {
    let accepted = vsock.listen(MEMORY_PORT);
    std::thread::Builder::new()
        .name("memory-guest".into())
        .spawn(move || {
            for crate::virtio::vsock::Accepted { key } in accepted {
                while let Some(bytes) = vsock.read_outbound_exact(key, 16) {
                    let word = |i: usize| {
                        u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])
                    };
                    let spare_mib = u64::from(word(0));
                    let release = word(12) != 0;
                    steering.guest_offers(spare_mib, release);
                }
                // The agent went away: whatever it offered is withdrawn.
                steering.guest_offers(0, true);
            }
        })
        .map(|_| ())
        .map_err(|e| format!("cannot start the guest memory listener: {e}"))
}

/// The steering rule, as arithmetic: a step up while the host compresses,
/// a smaller step down once it has been quiet for a few seconds, and
/// nothing in between. Quiet, not free: an 8 GB Mac shows a few hundred
/// megabytes free at the best of times, and a deflate that waited for more
/// left the balloon inflated through the install after the big one, which
/// then paid for the cache it did not have.
const QUIET_POLLS_BEFORE_DEFLATE: u32 = 5;

fn steer(pages: u32, compressed: u64, quiet_for: u32, cap: u32) -> u32 {
    if compressed >= COMPRESSING_BYTES_PER_POLL {
        pages
            .saturating_add((INFLATE_STEP_BYTES / BALLOON_PAGE_SIZE) as u32)
            .min(cap)
    } else if quiet_for >= QUIET_POLLS_BEFORE_DEFLATE {
        pages.saturating_sub((DEFLATE_STEP_BYTES / BALLOON_PAGE_SIZE) as u32)
    } else {
        pages
    }
}

/// The pressure-level half of the policy.
struct Levels(Arc<Steering>);

impl Observer for Levels {
    fn pressure(&self, level: Pressure) {
        let steering = &self.0;
        let wanted_bytes = match level {
            Pressure::Normal => 0,
            Pressure::Warn => steering.ram_bytes / WARN_FRACTION,
            Pressure::Critical => steering.ram_bytes / CRITICAL_FRACTION,
        };
        let pages = (wanted_bytes / BALLOON_PAGE_SIZE).min(u64::from(u32::MAX)) as u32;
        steering.level.store(level as u32, Ordering::Relaxed);
        if pages == steering.level_pages.swap(pages, Ordering::Relaxed) {
            return;
        }
        tracing::info!(
            pressure = ?level,
            reclaim_mib = wanted_bytes / (1 << 20),
            "host memory pressure changed"
        );
        steering.apply();
    }
}

/// What the host has compressed, read the way `vm_stat` reads it.
struct HostMemory {
    page: u64,
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

impl HostMemory {
    fn new() -> HostMemory {
        // SAFETY: a plain query with no pointers of ours involved.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
        HostMemory { page }
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
        Some(HostSample {
            compressed: stats.compressions * self.page,
        })
    }
}

/// One reading: how much the host has compressed since boot.
#[derive(Clone, Copy)]
struct HostSample {
    compressed: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three levels have to be ordered, because a policy that treats
    /// `Warn` and `Critical` the same either overreacts to a browser tab or
    /// underreacts to the machine swapping.
    #[test]
    fn the_levels_are_ordered_and_distinct() {
        assert!(Pressure::Normal < Pressure::Warn);
        assert!(Pressure::Warn < Pressure::Critical);
        const { assert!(WARN_FRACTION > CRITICAL_FRACTION) };
    }

    /// The arithmetic, without a hypervisor: an 8 GiB guest, and what each
    /// level asks it to give up.
    #[test]
    fn each_level_asks_for_a_sensible_share() {
        let ram: u64 = 8 << 30;
        let pages = |bytes: u64| bytes / BALLOON_PAGE_SIZE;
        assert_eq!(pages(ram / WARN_FRACTION), pages(2 << 30));
        assert_eq!(pages(ram / CRITICAL_FRACTION), pages(4 << 30));
        // And the page count has to fit the field the guest reads it from.
        assert!(pages(ram / CRITICAL_FRACTION) <= u64::from(u32::MAX));
    }

    /// The steering rule: a step up while compressing, a smaller step down
    /// once quiet for long enough, and a hold in between; never past the
    /// cap or below zero.
    #[test]
    fn steering_steps_up_while_compressing_and_eases_when_quiet() {
        let cap = ((512u64 << 20) / BALLOON_PAGE_SIZE) as u32;
        let up = steer(0, 64 << 20, 0, cap);
        assert_eq!(u64::from(up) * BALLOON_PAGE_SIZE, INFLATE_STEP_BYTES);
        assert_eq!(steer(up, 0, 2, cap), up, "quiet but not for long: held");
        assert_eq!(
            steer(up, 1 << 20, 0, cap),
            up,
            "a trickle, so no quiet yet: held"
        );
        let down = steer(up, 0, QUIET_POLLS_BEFORE_DEFLATE, cap);
        assert_eq!(u64::from(up - down) * BALLOON_PAGE_SIZE, DEFLATE_STEP_BYTES);
        assert_eq!(steer(cap, 1 << 30, 0, cap), cap, "never past the cap");
        assert_eq!(steer(1, 0, 30, cap), 0, "never below zero");
    }

    /// The statistics struct is the kernel's, integer for integer.
    #[test]
    fn the_statistics_struct_is_the_kernels_size() {
        assert_eq!(std::mem::size_of::<VmStatistics64>() / 4, 38);
        assert!(HostMemory::new().sample().is_some());
    }
}
