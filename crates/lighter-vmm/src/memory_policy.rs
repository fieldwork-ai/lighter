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

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

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

/// The pressure levels are all the host-side policy uses, and that is a
/// measured decision, not a default. An 8 GB Mac with a 4 GiB guest whose
/// page cache had filled with three package trees reported `Normal` the
/// whole way through while its compressor grew from 600 MB to 2 GB eating
/// the guest's pages, and the install after the big one paid fifteen
/// percent. Every cure tried cost more than that. Inflating the balloon on
/// compression handed back scattered 4 KiB pages, of which only aligned
/// runs of four are a host page on Apple silicon, so the guest lost its
/// cache and the host's footprint did not move. Asking the guest to reclaim
/// (`memory.reclaim` on its containers' cgroup, over the control channel)
/// whenever the compressor moved doubled the install; asking only when it
/// moved a quarter gigabyte in a second still fired eight times through
/// two installs and made the first repetition of each take twice as long,
/// against no requests at all: pnpm 12.7 s against 7.4, yarn 15.2 against
/// 10.6. A bound on the guest's cache, throttled or reclaimed, had the same
/// shape. An install's working set is the cache, and anything that takes
/// it mid-install is paid on every page. What the guest does on its own —
/// trim the cache once its containers have been idle half a minute — is
/// in the agent.
///
/// The guest port the agent's control channel listens on.
pub const AGENT_CONTROL_PORT: u32 = 2376;

/// The balloon, and the pressure signal that drives it.
pub struct MemoryPolicy {
    /// Held for the machine's lifetime; dropping it stops the subscription.
    _watcher: Watcher,
    level: Arc<AtomicU32>,
}

impl MemoryPolicy {
    /// Starts watching, and steering.
    pub fn start(
        balloon: Arc<BalloonState>,
        transport: Arc<Mutex<VirtioMmio>>,
        ram_bytes: u64,
    ) -> Result<MemoryPolicy, String> {
        let level = Arc::new(AtomicU32::new(Pressure::Normal as u32));
        let watcher = Watcher::start(Box::new(Steering {
            balloon,
            transport,
            ram_bytes,
            level: level.clone(),
        }))?;
        Ok(MemoryPolicy {
            _watcher: watcher,
            level,
        })
    }

    /// The last level the host reported. Diagnostics.
    pub fn level(&self) -> u32 {
        self.level.load(Ordering::Relaxed)
    }
}

struct Steering {
    balloon: Arc<BalloonState>,
    transport: Arc<Mutex<VirtioMmio>>,
    ram_bytes: u64,
    level: Arc<AtomicU32>,
}

impl Observer for Steering {
    fn pressure(&self, level: Pressure) {
        let wanted_bytes = match level {
            Pressure::Normal => 0,
            Pressure::Warn => self.ram_bytes / WARN_FRACTION,
            Pressure::Critical => self.ram_bytes / CRITICAL_FRACTION,
        };
        let pages = (wanted_bytes / BALLOON_PAGE_SIZE).min(u64::from(u32::MAX)) as u32;
        if pages == self.balloon.target_pages() {
            return;
        }
        self.balloon.set_target_pages(pages);
        self.level.store(level as u32, Ordering::Relaxed);
        // The guest only reads the target when told the configuration changed,
        // so this is not bookkeeping — it is the whole delivery mechanism.
        self.transport
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
}
