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
        [3 * TICKS_PER_SEC, 8 * TICKS_PER_SEC]
            .into_iter()
            .any(|threshold| before < threshold && self.ticks >= threshold)
    }

    pub fn elapsed_ticks(&self) -> u32 {
        self.ticks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

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
        for second in 1..=30 {
            if trim.tick(populated(&group.0), true, TICKS_PER_SEC) {
                passes.push(second);
            }
        }
        assert_eq!(passes, [3, 8]);
    }

    #[test]
    fn polling_interval_changes_cannot_skip_or_repeat_a_trim() {
        let mut trim = IdleTrim::default();
        assert!(!trim.tick(false, true, 11));
        assert!(trim.tick(false, true, 4));
        assert!(!trim.tick(false, true, 16));
        assert!(trim.tick(false, true, 4));
        assert!(!trim.tick(false, true, 4));
        assert!(!trim.tick(false, true, u32::MAX));
        assert!(!trim.tick(false, true, 1));
    }

    #[test]
    fn new_work_or_teardown_cpu_restarts_the_empty_interval() {
        for (populated, idle) in [(true, true), (false, false)] {
            let mut trim = IdleTrim::default();
            assert!(!trim.tick(false, true, 11));
            assert!(!trim.tick(populated, idle, 4));
            assert_eq!(trim.elapsed_ticks(), 0);
            assert!(!trim.tick(false, true, 11));
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
