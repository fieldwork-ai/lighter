//! The hourly pass over idle page cache.
//!
//! A cached file nobody has read or written for an hour is memory the Mac
//! could have. Nothing else in the guest ages memory while containers run:
//! the trims wait for an empty container hierarchy, the kernel's LRU only
//! moves under pressure, and the host's ramp only answers a Mac that is short
//! now. A daily stack left twelve hours of image-build cache, five gigabytes
//! charged to a cgroup with no processes, sitting in macOS's swap file.
//!
//! The pass is DAMON's, configured once through sysfs: one physical-address
//! context over the guest's RAM, one `pageout` scheme applied every horizon,
//! with three per-page filters. Anonymous pages are never candidates (the
//! trims' rule: a workload's heap is never swapped behind its back). Mapped
//! pages are never candidates: a sleeping process keeps its text and its
//! mapped files. Of the unmapped file cache that remains, the `young` filter
//! asks each page frame whether it was touched since the previous pass —
//! `PG_idle`, which every cached read and write clears — skips it and marks it
//! old if so, and evicts it if not. So a page goes at the first pass it
//! reaches untouched since the pass before: idle for at least the horizon and
//! at most twice it, decided per page from the kernel's own access bits.
//! DAMON's region sampling, which estimates hot and cold ranges, plays no
//! part in the decision; it is set slow (ten pages a minute) and ignored.
//!
//! Not the multi-generational LRU: its generations place a newly cached
//! read() page in the oldest one and never promote it on access, so they
//! measure how often a page was used, not when. Not `DAMON_RECLAIM`: its
//! cold-region rule is the sampled estimate.
//!
//! What "evicted" means: clean file cache is dropped, dirty file cache is
//! written first, and tmpfs pages, swap-backed but not anonymous, go to the
//! guest's zram (bounded by its size; pages it cannot take stay). The freed
//! pages return to the host through free page reporting, fed by a compaction
//! pass: on the M1's gate, 2227 of 2258 MiB evicted were back with the host
//! fifteen seconds later, with the virtio-mem range in and no balloon.

use std::path::Path;

/// How long a page must go untouched before the pass takes it.
pub const IDLE_HORIZON_SECS: u64 = 3600;

const ADMIN: &str = "/sys/kernel/mm/damon/admin";

/// DAMON's sampling interval, which must not exceed its aggregation
/// interval (the horizon): a minute, or a quarter of a short horizon.
pub fn sample_secs(horizon_secs: u64) -> u64 {
    (horizon_secs / 4).clamp(1, 60)
}

/// The span of guest physical memory to pass over: from the lowest
/// `System RAM` start to the highest end in `/proc/iomem`, the virtio-mem
/// device's parent resource included, so the range is covered whether it
/// is plugged now or later (DAMON skips frames that are not online).
pub fn span(iomem: &str) -> Option<(u64, u64)> {
    let mut span: Option<(u64, u64)> = None;
    for line in iomem.lines() {
        let Some((range, name)) = line.split_once(" : ") else { continue };
        if !name.starts_with("System RAM") {
            continue;
        }
        let Some((start, end)) = range.trim().split_once('-') else { continue };
        let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
        else {
            continue;
        };
        let end = end + 1;
        span = Some(match span {
            Some((s, e)) => (s.min(start), e.max(end)),
            None => (start, end),
        });
    }
    span
}

/// The sysfs writes that set the pass up, in order, relative to `root`
/// (DAMON's `admin` directory). `None` when there is no RAM to pass over.
pub fn configuration(root: &str, horizon_secs: u64, iomem: &str) -> Option<Vec<(String, String)>> {
    let (start, end) = span(iomem)?;
    let horizon_us = horizon_secs * 1_000_000;
    let ctx = format!("{root}/kdamonds/0/contexts/0");
    let scheme = format!("{ctx}/schemes/0");
    let mut writes = vec![
        (format!("{root}/kdamonds/nr_kdamonds"), "1".into()),
        (format!("{root}/kdamonds/0/contexts/nr_contexts"), "1".into()),
        (format!("{ctx}/operations"), "paddr".into()),
        (
            format!("{ctx}/monitoring_attrs/intervals/sample_us"),
            (sample_secs(horizon_secs) * 1_000_000).to_string(),
        ),
        (format!("{ctx}/monitoring_attrs/intervals/aggr_us"), horizon_us.to_string()),
        (format!("{ctx}/monitoring_attrs/intervals/update_us"), horizon_us.to_string()),
        (format!("{ctx}/monitoring_attrs/nr_regions/min"), "10".into()),
        (format!("{ctx}/monitoring_attrs/nr_regions/max"), "10".into()),
        (format!("{ctx}/targets/nr_targets"), "1".into()),
        (format!("{ctx}/targets/0/regions/nr_regions"), "1".into()),
        (format!("{ctx}/targets/0/regions/0/start"), start.to_string()),
        (format!("{ctx}/targets/0/regions/0/end"), end.to_string()),
        (format!("{ctx}/schemes/nr_schemes"), "1".into()),
        (format!("{scheme}/action"), "pageout".into()),
        (format!("{scheme}/apply_interval_us"), horizon_us.to_string()),
        // Every region: the region layer must not pre-filter.
        (format!("{scheme}/access_pattern/sz/min"), "0".into()),
        (format!("{scheme}/access_pattern/sz/max"), u64::MAX.to_string()),
        (format!("{scheme}/access_pattern/nr_accesses/min"), "0".into()),
        (format!("{scheme}/access_pattern/nr_accesses/max"), u32::MAX.to_string()),
        (format!("{scheme}/access_pattern/age/min"), "0".into()),
        (format!("{scheme}/access_pattern/age/max"), u32::MAX.to_string()),
        // No quota: a pass finishes. No watermark: always on.
        (format!("{scheme}/quotas/ms"), "0".into()),
        (format!("{scheme}/quotas/bytes"), "0".into()),
        (format!("{scheme}/watermarks/metric"), "none".into()),
        (format!("{scheme}/ops_filters/nr_filters"), "3".into()),
    ];
    // In this order: the young check's rmap walk, which clears PTE accessed
    // bits, never runs on a live mapping. A reject filter last means a page
    // no filter matched is allowed: unmapped, not anonymous, untouched.
    for (i, (kind, matching)) in [("anon", "Y"), ("unmapped", "N"), ("young", "Y")].into_iter().enumerate() {
        writes.push((format!("{scheme}/ops_filters/{i}/type"), kind.into()));
        writes.push((format!("{scheme}/ops_filters/{i}/matching"), matching.into()));
        writes.push((format!("{scheme}/ops_filters/{i}/allow"), "N".into()));
    }
    writes.push((format!("{root}/kdamonds/0/state"), "on".into()));
    Some(writes)
}

/// The horizon as the log says it.
pub fn horizon_text(horizon_secs: u64) -> String {
    if horizon_secs % 3600 == 0 {
        format!("{} h", horizon_secs / 3600)
    } else {
        format!("{horizon_secs} s")
    }
}

/// The pass, running: what it has evicted so far, read on a schedule.
pub struct Idle {
    root: String,
    horizon_secs: u64,
    /// `sz_applied` at the last read, in bytes.
    applied: u64,
    /// Ticks until the next read of the stats.
    until_poll: u32,
    poll_ticks: u32,
    running: bool,
}

impl Idle {
    /// Configures and starts the pass, or says once why it could not
    /// (`horizon_secs` 0 leaves DAMON alone; a kernel without it gets the
    /// line and 0.5.6's behaviour).
    pub fn start(horizon_secs: u64, ticks_per_sec: u32) -> Idle {
        Self::start_at(ADMIN, "/proc/iomem", horizon_secs, ticks_per_sec)
    }

    fn start_at(root: &str, iomem_path: &str, horizon_secs: u64, ticks_per_sec: u32) -> Idle {
        let poll_ticks = (sample_secs(horizon_secs.max(1)) as u32).saturating_mul(ticks_per_sec).max(1);
        let mut idle = Idle {
            root: root.into(),
            horizon_secs,
            applied: 0,
            until_poll: poll_ticks,
            poll_ticks,
            running: false,
        };
        if horizon_secs == 0 {
            return idle;
        }
        if !Path::new(root).join("kdamonds").is_dir() {
            println!("AGENT idle: DAMON unavailable");
            return idle;
        }
        let iomem = std::fs::read_to_string(iomem_path).unwrap_or_default();
        let Some(writes) = configuration(root, horizon_secs, &iomem) else {
            println!("AGENT idle: DAMON unavailable (no System RAM in iomem)");
            return idle;
        };
        for (path, value) in &writes {
            if let Err(e) = std::fs::write(path, value) {
                println!("AGENT idle: DAMON unavailable ({path}: {e})");
                return idle;
            }
        }
        idle.running = true;
        println!(
            "AGENT idle: pass every {} over unmapped page cache untouched since the last",
            horizon_text(horizon_secs)
        );
        idle
    }

    pub fn running(&self) -> bool {
        self.running
    }

    pub fn horizon_secs(&self) -> u64 {
        self.horizon_secs
    }

    /// One tick of the memory loop. Returns what the pass evicted since the
    /// last read, in bytes, when the stats were due and something moved.
    pub fn tick(&mut self, step: u32) -> Option<u64> {
        if !self.running {
            return None;
        }
        self.until_poll = self.until_poll.saturating_sub(step);
        if self.until_poll > 0 {
            return None;
        }
        self.until_poll = self.poll_ticks;
        let state = format!("{}/kdamonds/0/state", self.root);
        if std::fs::write(&state, "update_schemes_stats").is_err() {
            return None;
        }
        let applied = std::fs::read_to_string(format!(
            "{}/kdamonds/0/contexts/0/schemes/0/stats/sz_applied",
            self.root
        ))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())?;
        let delta = applied.saturating_sub(self.applied);
        self.applied = applied;
        (delta > 0).then_some(delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IOMEM: &str = "\
00000000-3fffffff : reserved
40000000-bfffffff : System RAM
  40010000-41a0ffff : Kernel code
  41a10000-41f5ffff : reserved
c0000000-c000ffff : virtio-mmio
100000000-4ffffffff : System RAM (virtio_mem)
  100000000-13fffffff : System RAM
";

    #[test]
    fn the_span_covers_the_base_and_the_whole_range() {
        assert_eq!(span(IOMEM), Some((0x4000_0000, 0x5_0000_0000)));
        assert_eq!(span("00000000-3fffffff : reserved\n"), None);
        assert_eq!(span(""), None);
    }

    #[test]
    fn the_sampling_interval_stays_under_the_horizon() {
        assert_eq!(sample_secs(3600), 60);
        assert_eq!(sample_secs(40), 10);
        assert_eq!(sample_secs(3), 1);
    }

    #[test]
    fn the_configuration_is_one_pageout_scheme_with_three_filters_in_order() {
        let writes = configuration("/d", 3600, IOMEM).unwrap();
        let get = |p: &str| writes.iter().find(|(k, _)| k == &format!("/d/{p}")).map(|(_, v)| v.as_str());
        assert_eq!(get("kdamonds/0/contexts/0/operations"), Some("paddr"));
        assert_eq!(get("kdamonds/0/contexts/0/monitoring_attrs/intervals/sample_us"), Some("60000000"));
        assert_eq!(get("kdamonds/0/contexts/0/monitoring_attrs/intervals/aggr_us"), Some("3600000000"));
        assert_eq!(get("kdamonds/0/contexts/0/targets/0/regions/0/start"), Some("1073741824"));
        assert_eq!(get("kdamonds/0/contexts/0/targets/0/regions/0/end"), Some("21474836480"));
        let s = "kdamonds/0/contexts/0/schemes/0";
        assert_eq!(get(&format!("{s}/action")), Some("pageout"));
        assert_eq!(get(&format!("{s}/apply_interval_us")), Some("3600000000"));
        assert_eq!(get(&format!("{s}/quotas/ms")), Some("0"));
        assert_eq!(get(&format!("{s}/watermarks/metric")), Some("none"));
        assert_eq!(get(&format!("{s}/ops_filters/nr_filters")), Some("3"));
        let filters: Vec<(&str, &str, &str)> = (0..3)
            .map(|i| {
                (
                    get(&format!("{s}/ops_filters/{i}/type")).unwrap(),
                    get(&format!("{s}/ops_filters/{i}/matching")).unwrap(),
                    get(&format!("{s}/ops_filters/{i}/allow")).unwrap(),
                )
            })
            .collect();
        assert_eq!(filters, [("anon", "Y", "N"), ("unmapped", "N", "N"), ("young", "Y", "N")]);
        // The last write turns it on, after everything is in place.
        assert_eq!(writes.last().unwrap(), &("/d/kdamonds/0/state".to_string(), "on".to_string()));
        // The scheme comes after the target and the attrs, the filters after the scheme.
        let pos = |p: &str| writes.iter().position(|(k, _)| k == &format!("/d/{p}")).unwrap();
        assert!(pos("kdamonds/0/contexts/0/targets/0/regions/0/end") < pos(&format!("{s}/action")));
        assert!(pos(&format!("{s}/ops_filters/nr_filters")) < pos(&format!("{s}/ops_filters/0/type")));
    }

    #[test]
    fn a_short_horizon_scales_the_sampling_and_no_ram_means_no_configuration() {
        let writes = configuration("/d", 40, IOMEM).unwrap();
        let get = |p: &str| writes.iter().find(|(k, _)| k == &format!("/d/{p}")).map(|(_, v)| v.as_str());
        assert_eq!(get("kdamonds/0/contexts/0/monitoring_attrs/intervals/sample_us"), Some("10000000"));
        assert_eq!(get("kdamonds/0/contexts/0/schemes/0/apply_interval_us"), Some("40000000"));
        assert!(configuration("/d", 40, "").is_none());
    }

    #[test]
    fn the_horizon_reads_as_hours_or_seconds() {
        assert_eq!(horizon_text(3600), "1 h");
        assert_eq!(horizon_text(7200), "2 h");
        assert_eq!(horizon_text(40), "40 s");
    }

    #[test]
    fn without_damon_the_pass_does_not_run_and_a_zero_horizon_leaves_it_alone() {
        let dir = std::env::temp_dir().join(format!("lighter-idle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.to_str().unwrap();
        let mut idle = Idle::start_at(root, "/nonexistent", 3600, 4);
        assert!(!idle.running());
        assert_eq!(idle.tick(4), None);
        let idle = Idle::start_at(root, "/nonexistent", 0, 4);
        assert!(!idle.running());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_stats_are_read_on_the_sampling_cadence_and_only_growth_is_reported() {
        let dir = std::env::temp_dir().join(format!("lighter-idle-stats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let scheme = dir.join("kdamonds/0/contexts/0/schemes/0/stats");
        std::fs::create_dir_all(&scheme).unwrap();
        std::fs::write(scheme.join("sz_applied"), "0\n").unwrap();
        std::fs::write(dir.join("kdamonds/0/state"), "off\n").unwrap();
        let root = dir.to_str().unwrap().to_string();
        let mut idle = Idle {
            root: root.clone(),
            horizon_secs: 40,
            applied: 0,
            until_poll: 40,
            poll_ticks: 40,
            running: true,
        };
        // Ten seconds at four ticks a second before the first read.
        assert_eq!(idle.tick(36), None);
        std::fs::write(scheme.join("sz_applied"), format!("{}\n", 3u64 << 30)).unwrap();
        assert_eq!(idle.tick(4), Some(3 << 30));
        // The read asked DAMON to refresh its stats first.
        assert_eq!(std::fs::read_to_string(dir.join("kdamonds/0/state")).unwrap(), "update_schemes_stats");
        // Nothing new: nothing reported, and the next read is a full cadence away.
        assert_eq!(idle.tick(40), None);
        std::fs::write(scheme.join("sz_applied"), format!("{}\n", 4u64 << 30)).unwrap();
        assert_eq!(idle.tick(39), None);
        assert_eq!(idle.tick(1), Some(1 << 30));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
