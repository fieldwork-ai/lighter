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

/// The pressure levels are not the whole signal. An 8 GB Mac with a 4 GiB
/// guest whose page cache has filled with three package trees reported
/// `Normal` the whole way through, while free memory sat at sixty megabytes
/// and the compressor grew from six hundred megabytes to two gigabytes:
/// macOS compresses first and reports pressure later, and by then the
/// guest's pages are the ones being compressed. OrbStack's footprint on the
/// same run stayed at two gigabytes and its installs did not slow down; ours
/// held four to six and the install after the big one paid fifteen percent.
///
/// Nor is "available memory" the signal: macOS keeps the guest's own pages
/// on its inactive list, so free plus inactive read two gigabytes while the
/// compressor was eating the guest. The compressor is the signal — pages
/// being compressed is the exact cost this exists to avoid.
///
/// And the balloon is the wrong tool for it. Inflated, it hands back
/// scattered 4 KiB guest pages, of which only aligned runs of four are a
/// host page on Apple silicon: measured, the guest lost three quarters of a
/// gigabyte of cache and the host's footprint did not move, and the install
/// that followed took twice as long. So while the host is compressing, the
/// guest is asked to reclaim from its containers' cgroup instead — the
/// kernel drops the coldest page cache first, in bulk — and free page
/// reporting returns what it freed in runs the host can take. The ask is
/// twice what the host just compressed, within bounds, one per second.
///
/// This is the backstop. The standing measure is the guest's: its agent
/// bounds the containers' page cache to half of guest RAM (`memory.high`),
/// which is what keeps the compressor quiet in the first place — asked to
/// reclaim every second through an install, the guest gave back the cache
/// the install was using and it took twice as long. So the threshold is
/// distress, not housekeeping: the compressor taking a quarter gigabyte in
/// a second is the host swapping the guest out in all but name.
const COMPRESSING_BYTES_PER_POLL: u64 = 256 << 20;
const RECLAIM_MIN_BYTES: u64 = 256 << 20;
const RECLAIM_MAX_BYTES: u64 = 1 << 30;
const POLL: Duration = Duration::from_secs(1);

/// The guest port the agent's control channel listens on.
pub const AGENT_CONTROL_PORT: u32 = 2376;

/// The balloon, and the pressure signal that drives it.
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
    ) -> Result<MemoryPolicy, String> {
        let steering = Arc::new(Steering {
            balloon,
            transport,
            ram_bytes,
            level: AtomicU32::new(Pressure::Normal as u32),
            control: Mutex::new(None),
        });
        let watcher = Watcher::start(Box::new(Levels(steering.clone())))?;
        let stop = Arc::new(AtomicBool::new(false));
        let host = HostMemory::new();
        {
            let steering = steering.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("memory-policy".into())
                .spawn(move || {
                    let mut last = host.sample();
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(POLL);
                        let Some(now) = host.sample() else { continue };
                        if let Some(then) = last {
                            let compressed = now.compressed.saturating_sub(then.compressed);
                            if let Some(bytes) = ask_for(compressed) {
                                steering.reclaim(bytes, compressed);
                            }
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

    /// Where the guest agent's control channel is reachable from the host:
    /// the socket the machine proxies to [`AGENT_CONTROL_PORT`]. Without it
    /// the policy has the balloon and nothing else.
    pub fn set_control_socket(&self, path: &std::path::Path) {
        *self.steering.control.lock().expect("control path poisoned") = Some(path.to_path_buf());
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

struct Steering {
    balloon: Arc<BalloonState>,
    transport: Arc<Mutex<VirtioMmio>>,
    ram_bytes: u64,
    level: AtomicU32,
    control: Mutex<Option<std::path::PathBuf>>,
}

impl Steering {
    fn reclaim(&self, bytes: u64, compressed: u64) {
        let Some(path) = self.control.lock().expect("control path poisoned").clone() else {
            return;
        };
        let mib = bytes >> 20;
        match request_reclaim(&path, mib) {
            Ok(answer) => tracing::info!(
                compressed_mib = compressed >> 20,
                asked_mib = mib,
                answer,
                "host is compressing; the guest was asked to reclaim"
            ),
            Err(e) => {
                tracing::warn!(%e, asked_mib = mib, "the guest could not be asked to reclaim")
            }
        }
    }
}

/// How much to ask the guest for, given what the host compressed in the
/// last poll: nothing below the housekeeping level, else twice that, within
/// bounds.
fn ask_for(compressed: u64) -> Option<u64> {
    if compressed < COMPRESSING_BYTES_PER_POLL {
        return None;
    }
    Some((compressed * 2).clamp(RECLAIM_MIN_BYTES, RECLAIM_MAX_BYTES))
}

/// One line to the agent, one line back. The reclaim itself is synchronous
/// in the guest, so the answer can take a moment; this runs on the policy's
/// own thread and nothing waits on it.
fn request_reclaim(path: &std::path::Path, mib: u64) -> std::io::Result<String> {
    use std::io::{BufRead, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(format!("reclaim {mib}\n").as_bytes())?;
    let mut reply = String::new();
    std::io::BufReader::new(&stream).read_line(&mut reply)?;
    Ok(reply.trim().to_string())
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
        if pages == steering.balloon.target_pages() {
            return;
        }
        steering.balloon.set_target_pages(pages);
        // The guest only reads the target when told the configuration changed,
        // so this is not bookkeeping — it is the whole delivery mechanism.
        steering
            .transport
            .lock()
            .expect("balloon transport poisoned")
            .notify_config_change();
        tracing::info!(
            pressure = ?level,
            reclaim_mib = wanted_bytes / (1 << 20),
            "host memory pressure changed; balloon target updated"
        );
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

    /// The ask: nothing for housekeeping, twice the compression otherwise,
    /// within bounds.
    #[test]
    fn the_ask_follows_the_compression_within_bounds() {
        assert_eq!(ask_for(1 << 20), None);
        assert_eq!(ask_for(128 << 20), None);
        assert_eq!(ask_for(256 << 20), Some(512 << 20));
        assert_eq!(ask_for(4 << 30), Some(RECLAIM_MAX_BYTES));
    }

    /// The statistics struct is the kernel's, integer for integer.
    #[test]
    fn the_statistics_struct_is_the_kernels_size() {
        assert_eq!(std::mem::size_of::<VmStatistics64>() / 4, 38);
        let host = HostMemory::new();
        assert!(host.sample().is_some());
    }
}
