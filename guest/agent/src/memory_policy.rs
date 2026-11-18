//! Population and idle trimming are independent of the guest transport.

use std::path::Path;

pub const TICKS_PER_SEC: u32 = 4;

/// cgroup.events includes descendants, unlike cgroup.procs. Unknown state
/// must not permit a trim or tell the host that no workload needs memory.
pub fn populated(cgroup: &Path) -> bool {
    std::fs::read_to_string(cgroup.join("cgroup.events"))
        .ok()
        .and_then(|events| population(&events))
        .unwrap_or(true)
}

fn population(events: &str) -> Option<bool> {
    let mut value = None;
    for line in events.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("populated") {
            continue;
        }
        if value.is_some() {
            return None;
        }
        value = match fields.next()? {
            "0" => Some(false),
            "1" => Some(true),
            _ => return None,
        };
        if fields.next().is_some() {
            return None;
        }
    }
    value
}

/// When an empty, idle hierarchy's cache is trimmed, in ticks: a second pass
/// five seconds after the first takes what the first freed only after. Half
/// a minute, so the next command of a script or of a person still finds the
/// tree the last one wrote in the guest: at three seconds a build reading an
/// install's tree over the share read all of it from the Mac again (kernel
/// patches 0043-0045 keep it only while the cache does).
pub const TRIM_AFTER: [u32; 2] = [30 * TICKS_PER_SEC, 35 * TICKS_PER_SEC];

/// Two trims after the hierarchy becomes empty and idle. CPU idleness alone
/// says nothing about whether a process still needs its mapped file pages.
#[derive(Default)]
pub struct IdleTrim {
    ticks: u32,
}

impl IdleTrim {
    pub fn tick(&mut self, populated: bool, cpu_idle: bool, step: u32) -> bool {
        let before = self.ticks;
        self.ticks = if populated || !cpu_idle {
            0
        } else {
            before.saturating_add(step)
        };
        TRIM_AFTER
            .into_iter()
            .any(|threshold| before < threshold && self.ticks >= threshold)
    }

    pub fn elapsed_ticks(&self) -> u32 {
        self.ticks
    }
}

/// Whether the guest as a whole ran short since the last look, from
/// `/proc/vmstat`: the page allocator entering direct reclaim
/// (`allocstall_*`), which the kernel counts only for reclaim that is not a
/// cgroup's (`do_try_to_free_pages`, `!cgroup_reclaim`). Pressure stall
/// information cannot tell the two apart: a container at its own
/// `memory.max` reclaims under `psi_memstall_enter` like a guest out of
/// memory, and a pressure-only need grew a guest from 28 to 47 GiB on a Mac
/// at Critical for a builder at its 12 GiB limit (0.10.2, 2026-09-30), and
/// on the M1 took a guest from 384 MiB to its whole range in a second for a
/// container thrashing at 1 GiB, with these counters at zero.
///
/// Not while the balloon inflated: its allocations enter direct reclaim as
/// well (`__GFP_NORETRY` allows one pass), and a need on that would hand the
/// balloon straight back.
#[derive(Default)]
pub struct GlobalReclaim {
    last: Option<(u64, u64)>,
}

impl GlobalReclaim {
    pub fn tick(&mut self, vmstat: &str) -> bool {
        let (mut stalls, mut inflated) = (0u64, 0u64);
        for line in vmstat.lines() {
            let mut fields = line.split_whitespace();
            let (Some(name), Some(value)) = (fields.next(), fields.next()) else { continue };
            let Ok(value) = value.parse::<u64>() else { continue };
            if name.starts_with("allocstall_") {
                stalls += value;
            } else if name == "balloon_inflate" {
                inflated = value;
            }
        }
        let Some((last_stalls, last_inflated)) = self.last.replace((stalls, inflated)) else {
            return false;
        };
        stalls > last_stalls && inflated == last_inflated
    }
}

/// What free page reporting should be set to while no trim has it hurried:
/// the rest order, the churn order, or a drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reporting {
    /// The rest order at the slow pace: a guest ticking over.
    Rest,
    /// The kernel's two megabytes, while the containers work: reporting
    /// smaller runs then hands back what an install is about to take again.
    Churn,
    /// Everything free down to the rest order, at once, then the churn order
    /// again: after a minute at the churn order, so freed memory waits a
    /// minute at most however the load is judged (#112).
    Drain,
}

/// The rest order until the containers use half a core in a tick, the churn
/// order until they have been under that for eight seconds, and a drain each
/// minute the churn order lasts. A load that is never eight seconds light,
/// as a stack of agent sandboxes with a heavy tick every second or two is
/// not, never reaches the rest order, and kept the Mac holding what it freed
/// in runs under two megabytes for 35 hours (#112).
#[derive(Debug)]
pub struct ReportingOrder {
    at_rest: bool,
    /// Ticks at the churn order since it began or since the last drain.
    churned_for: u32,
    /// What is left of a drain under way.
    draining_for: u32,
}

/// Ticks at the churn order before a drain.
pub const DRAIN_EVERY: u32 = 60 * TICKS_PER_SEC;
/// How long a drain holds the rest order: one cycle at the hurried delay
/// takes each free list whole, so two seconds is many.
pub const DRAIN_FOR: u32 = 2 * TICKS_PER_SEC;
/// Light ticks in a row that make the containers at rest.
pub const REST_AFTER: u32 = 8 * TICKS_PER_SEC;

impl ReportingOrder {
    pub fn new(at_rest: bool) -> Self {
        ReportingOrder { at_rest, churned_for: 0, draining_for: 0 }
    }

    /// Starts over from a setting made elsewhere (the end of a trim's burst).
    pub fn reset(&mut self, at_rest: bool) {
        *self = ReportingOrder::new(at_rest);
    }

    /// One tick of `step` ticks: whether the containers used half a core in
    /// it, and for how many ticks they have not. What reporting changes to,
    /// if it changes.
    pub fn tick(&mut self, heavy: bool, light_for: u32, step: u32) -> Option<Reporting> {
        if self.at_rest {
            if heavy {
                *self = ReportingOrder::new(false);
                return Some(Reporting::Churn);
            }
            return None;
        }
        if light_for >= REST_AFTER {
            *self = ReportingOrder::new(true);
            return Some(Reporting::Rest);
        }
        if self.draining_for > 0 {
            self.draining_for = self.draining_for.saturating_sub(step);
            return (self.draining_for == 0).then_some(Reporting::Churn);
        }
        self.churned_for += step;
        if self.churned_for >= DRAIN_EVERY {
            self.churned_for = 0;
            self.draining_for = DRAIN_FOR;
            return Some(Reporting::Drain);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a run of ticks changes reporting to, by tick.
    fn run(order: &mut ReportingOrder, ticks: u32, heavy: impl Fn(u32) -> bool) -> Vec<(u32, Reporting)> {
        let mut light_for = 0;
        let mut changes = Vec::new();
        for t in 0..ticks {
            let h = heavy(t);
            light_for = if h { 0 } else { light_for + 1 };
            if let Some(r) = order.tick(h, light_for, 1) {
                changes.push((t, r));
            }
        }
        changes
    }

    /// The load of #112: a heavy tick every five, never eight seconds light.
    /// It churns, and drains every minute, each drain two seconds long.
    #[test]
    fn a_load_never_at_rest_drains_every_minute() {
        let mut order = ReportingOrder::new(true);
        let changes = run(&mut order, 5 * 60 * TICKS_PER_SEC, |t| t % 5 == 0);
        assert_eq!(changes[0], (0, Reporting::Churn));
        let drains: Vec<u32> = changes.iter().filter(|c| c.1 == Reporting::Drain).map(|c| c.0).collect();
        assert_eq!(drains.len(), 4, "{changes:?}");
        for pair in drains.windows(2) {
            assert_eq!(pair[1] - pair[0], DRAIN_EVERY + DRAIN_FOR, "a minute at the churn order between drains");
        }
        for drain in drains {
            assert!(changes.contains(&(drain + DRAIN_FOR, Reporting::Churn)), "the churn order again after {drain}");
        }
        assert!(!changes.iter().any(|c| c.1 == Reporting::Rest));
    }

    /// An install, heavy throughout for half a minute, then idle: churn, no
    /// drain, and rest eight seconds after.
    #[test]
    fn an_install_churns_without_a_drain_and_then_rests() {
        let mut order = ReportingOrder::new(true);
        let install = 30 * TICKS_PER_SEC;
        let changes = run(&mut order, install + 20 * TICKS_PER_SEC, |t| t < install);
        assert_eq!(changes, vec![(0, Reporting::Churn), (install - 1 + REST_AFTER, Reporting::Rest)]);
    }

    /// The light ticks of a drain count toward rest: a load that stops
    /// during a drain rests without the churn order coming back first.
    #[test]
    fn a_load_that_stops_mid_drain_rests() {
        let mut order = ReportingOrder::new(false);
        let mut changes = Vec::new();
        for t in 0..DRAIN_EVERY {
            if let Some(r) = order.tick(t % 5 == 0, 0, 1) {
                changes.push(r);
            }
        }
        assert_eq!(changes, vec![Reporting::Drain]);
        assert_eq!(order.tick(false, REST_AFTER, 1), Some(Reporting::Rest));
    }

    /// The idle tick steps by four: a drain still ends, and still comes.
    #[test]
    fn coarse_ticks_still_drain_and_end() {
        let mut order = ReportingOrder::new(false);
        let mut seen = Vec::new();
        for _ in 0..(DRAIN_EVERY + DRAIN_FOR) / 4 + 2 {
            if let Some(r) = order.tick(false, 0, 4) {
                seen.push(r);
            }
        }
        assert_eq!(seen, vec![Reporting::Drain, Reporting::Churn]);
    }
    use std::sync::atomic::{AtomicU64, Ordering};

    fn vmstat(normal: u64, movable: u64, inflate: u64) -> String {
        format!(
            "nr_free_pages 1000\nallocstall_dma 0\nallocstall_normal {normal}\nallocstall_movable {movable}\npgscan_direct 5\nballoon_inflate {inflate}\nballoon_deflate 3\n"
        )
    }

    #[test]
    fn direct_reclaim_in_any_zone_is_the_guest_short() {
        let mut reclaim = GlobalReclaim::default();
        assert!(!reclaim.tick(&vmstat(4, 9, 7)), "the first look is a baseline");
        assert!(!reclaim.tick(&vmstat(4, 9, 7)), "no stall since");
        assert!(reclaim.tick(&vmstat(4, 10, 7)), "the movable zone stalled");
        assert!(reclaim.tick(&vmstat(5, 10, 7)), "the kernel's zone stalled");
        assert!(!reclaim.tick(&vmstat(5, 10, 7)), "and stopped");
    }

    #[test]
    fn a_stall_while_the_balloon_inflates_is_the_balloons() {
        let mut reclaim = GlobalReclaim::default();
        reclaim.tick(&vmstat(0, 0, 100));
        assert!(!reclaim.tick(&vmstat(0, 3, 140)));
        assert!(reclaim.tick(&vmstat(0, 4, 140)), "the next stall with the balloon still is");
    }

    #[test]
    fn no_vmstat_is_never_short() {
        let mut reclaim = GlobalReclaim::default();
        assert!(!reclaim.tick(""));
        assert!(!reclaim.tick(""));
    }

    struct Group(std::path::PathBuf);

    impl Group {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "lighter-population-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn events(&self, events: &str) {
            std::fs::write(self.0.join("cgroup.events"), events).unwrap();
        }
    }

    impl Drop for Group {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn nested_process_keeps_idle_cache_and_live_memory_policy() {
        let group = Group::new();
        std::fs::write(group.0.join("cgroup.procs"), "").unwrap();
        let nested = group.0.join("buildx/builder/init");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("cgroup.procs"), "42\n").unwrap();
        group.events("populated 1\nfrozen 0\n");
        let mut trim = IdleTrim::default();
        for _ in 0..120 {
            assert!(populated(&group.0));
            assert!(!trim.tick(populated(&group.0), true, TICKS_PER_SEC));
        }
        assert_eq!(trim.elapsed_ticks(), 0);
    }

    #[test]
    fn last_exit_starts_a_new_empty_interval_even_after_long_cpu_idle() {
        let group = Group::new();
        group.events("populated 1\n");
        let mut trim = IdleTrim::default();
        assert!(!trim.tick(populated(&group.0), true, 120 * TICKS_PER_SEC));
        group.events("populated 0\nfrozen 0\n");
        let mut passes = Vec::new();
        for second in 1..=60 {
            if trim.tick(populated(&group.0), true, TICKS_PER_SEC) {
                passes.push(second);
            }
        }
        assert_eq!(passes, TRIM_AFTER.map(|t| t / TICKS_PER_SEC));
    }

    #[test]
    fn polling_interval_changes_cannot_skip_or_repeat_a_trim() {
        let [first, second] = TRIM_AFTER;
        let mut trim = IdleTrim::default();
        assert!(!trim.tick(false, true, first - 1));
        assert!(trim.tick(false, true, 4));
        assert!(!trim.tick(false, true, second - first - 4));
        assert!(trim.tick(false, true, 4));
        assert!(!trim.tick(false, true, 4));
        assert!(!trim.tick(false, true, u32::MAX));
        assert!(!trim.tick(false, true, 1));
    }

    #[test]
    fn new_work_or_teardown_cpu_restarts_the_empty_interval() {
        for (populated, idle) in [(true, true), (false, false)] {
            let mut trim = IdleTrim::default();
            assert!(!trim.tick(false, true, TRIM_AFTER[0] - 1));
            assert!(!trim.tick(populated, idle, 4));
            assert_eq!(trim.elapsed_ticks(), 0);
            assert!(!trim.tick(false, true, TRIM_AFTER[0] - 1));
            assert!(trim.tick(false, true, 1));
        }
    }

    #[test]
    fn unknown_population_preserves_workloads() {
        let group = Group::new();
        assert!(populated(&group.0));
        for invalid in [
            "",
            "frozen 0\n",
            "populated",
            "populated 2",
            "populated 0 extra",
            "populated 0\npopulated 1\n",
        ] {
            group.events(invalid);
            assert!(populated(&group.0), "{invalid:?}");
        }
        group.events("frozen 0\npopulated 0\n");
        assert!(!populated(&group.0));
    }
}
