//! Deciding how much memory to ask the guest for.
//!
//! Under the framework the balloon is the one channel memory goes back
//! through (no free page reporting; see `balloon.rs`), so the policy answers
//! one question: how large should the balloon be right now? Three inputs,
//! the largest wins:
//!
//! - macOS's own pressure levels, because reacting to the signal the kernel
//!   reacts to means reacting at the same moment;
//! - the compressor: while the Mac is compressing, a step up each second,
//!   and a smaller step down once it has been quiet;
//! - the guest's offers: after its trims and compaction the agent says what
//!   it can spare, and asks for it all back when it is short.
//!
//! The guest keeps at least a sixteenth of its RAM whatever is asked.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::balloon::Balloon;
use crate::mempressure::{Observer, Pressure, Watcher};

const WARN_FRACTION: u64 = 4; // a quarter
const CRITICAL_FRACTION: u64 = 2; // a half
const COMPRESSING_BYTES_PER_POLL: u64 = 64 << 20;
const INFLATE_STEP_BYTES: u64 = 32 << 20;
const DEFLATE_STEP_BYTES: u64 = 32 << 20;
const STEER_CAP_FRACTION: u64 = 8;
const POLL: Duration = Duration::from_secs(1);
const GUEST_RESERVE_FRACTION: u64 = 16;
const QUIET_POLLS_BEFORE_DEFLATE: u32 = 5;
/// One offer moves the balloon by at most this much.
const OFFER_STEP_BYTES: u64 = 8192 << 20;
/// Offers under this are noise.
const OFFER_FLOOR_BYTES: u64 = 64 << 20;

pub struct MemoryPolicy {
    _watcher: Watcher,
    steering: Arc<Steering>,
    stop: Arc<AtomicBool>,
}

impl MemoryPolicy {
    pub fn start(balloon: Arc<Balloon>, ram_bytes: u64) -> Result<MemoryPolicy, String> {
        let steering = Arc::new(Steering {
            balloon,
            ram_bytes,
            level: AtomicU32::new(Pressure::Normal as u32),
            level_bytes: AtomicU64::new(0),
            steer_bytes: AtomicU64::new(0),
            guest_bytes: AtomicU64::new(0),
        });
        let watcher = Watcher::start(Box::new(Levels(steering.clone())))?;
        let stop = Arc::new(AtomicBool::new(false));
        if let Some(mib) = std::env::var("LIGHTER_BALLOON_TEST_MIB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            steering.steer_bytes.store(mib << 20, Ordering::Relaxed);
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
                    let trace = std::env::var("LIGHTER_MEM_TRACE").is_ok_and(|v| v == "1");
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(POLL);
                        if trace {
                            let (resident, internal, reusable) = crate::footprint::split();
                            eprintln!(
                                "MEMTRACE footprint_mib={} resident_mib={} internal_mib={} reusable_mib={} offered_mib={} ballooned_mib={} steer_mib={} level_mib={}",
                                crate::footprint::bytes() >> 20,
                                resident >> 20,
                                internal >> 20,
                                reusable >> 20,
                                steering.balloon.offered_bytes() >> 20,
                                steering.balloon.ballooned_bytes() >> 20,
                                steering.steer_bytes.load(Ordering::Relaxed) >> 20,
                                steering.level_bytes.load(Ordering::Relaxed) >> 20
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

    pub fn level(&self) -> u32 {
        self.steering.level.load(Ordering::Relaxed)
    }

    /// The hook the link calls with the guest's offers.
    pub fn offers(&self) -> Arc<dyn Fn(u64, bool) + Send + Sync> {
        let steering = self.steering.clone();
        Arc::new(move |spare_mib, release| steering.guest_offers(spare_mib, release))
    }
}

impl Drop for MemoryPolicy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn steering_enabled() -> bool {
    std::env::var("LIGHTER_MEMORY_STEER")
        .ok()
        .is_none_or(|v| v != "0")
}

struct Steering {
    balloon: Arc<Balloon>,
    ram_bytes: u64,
    level: AtomicU32,
    level_bytes: AtomicU64,
    steer_bytes: AtomicU64,
    guest_bytes: AtomicU64,
}

impl Steering {
    fn cap_bytes(&self) -> u64 {
        self.ram_bytes / STEER_CAP_FRACTION
    }

    fn steer(&self, compressed: u64, quiet_for: u32) {
        let before = self.steer_bytes.load(Ordering::Relaxed);
        let after = steer(before, compressed, quiet_for, self.cap_bytes());
        if after != before {
            self.steer_bytes.store(after, Ordering::Relaxed);
            self.apply();
        }
    }

    /// The guest says it can spare `spare_mib` beyond what the balloon
    /// already holds; `release` asks the whole balloon back.
    fn guest_offers(&self, spare_mib: u64, release: bool) {
        let spare = spare_mib << 20;
        self.balloon.note_offered(spare);
        let cap = self.ram_bytes - self.ram_bytes / GUEST_RESERVE_FRACTION;
        let held = self.guest_bytes.load(Ordering::Relaxed);
        let wanted = if release {
            0
        } else if spare < OFFER_FLOOR_BYTES {
            return;
        } else {
            (held + spare.min(OFFER_STEP_BYTES)).min(cap)
        };
        if wanted != self.guest_bytes.swap(wanted, Ordering::Relaxed) {
            self.apply();
        }
    }

    fn apply(&self) {
        let bytes = self
            .level_bytes
            .load(Ordering::Relaxed)
            .max(self.steer_bytes.load(Ordering::Relaxed))
            .max(self.guest_bytes.load(Ordering::Relaxed));
        let before = self.balloon.ballooned_bytes();
        if bytes == before {
            return;
        }
        self.balloon.set_ballooned_bytes(bytes);
        if (before >> 28) != (bytes >> 28) {
            tracing::info!(target_mib = bytes >> 20, "balloon target");
        } else {
            tracing::debug!(target_mib = bytes >> 20, "balloon target");
        }
    }
}

fn steer(bytes: u64, compressed: u64, quiet_for: u32, cap: u64) -> u64 {
    if compressed >= COMPRESSING_BYTES_PER_POLL {
        bytes.saturating_add(INFLATE_STEP_BYTES).min(cap)
    } else if quiet_for >= QUIET_POLLS_BEFORE_DEFLATE {
        bytes.saturating_sub(DEFLATE_STEP_BYTES)
    } else {
        bytes
    }
}

struct Levels(Arc<Steering>);

impl Observer for Levels {
    fn pressure(&self, level: Pressure) {
        let steering = &self.0;
        let wanted = match level {
            Pressure::Normal => 0,
            Pressure::Warn => steering.ram_bytes / WARN_FRACTION,
            Pressure::Critical => steering.ram_bytes / CRITICAL_FRACTION,
        };
        steering.level.store(level as u32, Ordering::Relaxed);
        if wanted == steering.level_bytes.swap(wanted, Ordering::Relaxed) {
            return;
        }
        tracing::info!(
            pressure = ?level,
            reclaim_mib = wanted >> 20,
            "host memory pressure changed"
        );
        steering.apply();
    }
}

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
        // SAFETY: sysconf takes a constant.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
        HostMemory { page }
    }

    fn sample(&self) -> Option<HostSample> {
        let mut stats = VmStatistics64::default();
        let mut count = (std::mem::size_of::<VmStatistics64>() / 4) as u32;
        // SAFETY: host_statistics64 writes at most `count` words into the struct.
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

#[derive(Clone, Copy)]
struct HostSample {
    compressed: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_levels_are_ordered_and_distinct() {
        assert!(Pressure::Normal < Pressure::Warn);
        assert!(Pressure::Warn < Pressure::Critical);
        const { assert!(WARN_FRACTION > CRITICAL_FRACTION) };
    }

    #[test]
    fn steering_steps_up_while_compressing_and_eases_when_quiet() {
        let cap = 512u64 << 20;
        let up = steer(0, 64 << 20, 0, cap);
        assert_eq!(up, INFLATE_STEP_BYTES);
        assert_eq!(steer(up, 0, 2, cap), up, "quiet but not for long: held");
        assert_eq!(steer(up, 1 << 20, 0, cap), up, "a trickle, so no quiet yet: held");
        let down = steer(up, 0, QUIET_POLLS_BEFORE_DEFLATE, cap);
        assert_eq!(up - down, DEFLATE_STEP_BYTES);
        assert_eq!(steer(cap, 1 << 30, 0, cap), cap, "never past the cap");
        assert_eq!(steer(1, 0, 30, cap), 0, "never below zero");
    }

    #[test]
    fn the_statistics_struct_is_the_kernels_size() {
        assert_eq!(std::mem::size_of::<VmStatistics64>() / 4, 38);
        assert!(HostMemory::new().sample().is_some());
    }
}
