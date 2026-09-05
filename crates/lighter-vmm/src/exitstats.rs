//! Exit accounting, for finding out what a workload costs the VMM.
//!
//! Counters are process-wide and lock-free; a vCPU bumps one per exit. They
//! are read only by the reporter, which `LIGHTER_EXIT_STATS=1` turns on and
//! which logs the rate every two seconds. Off, the cost is one relaxed
//! increment per exit.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Kind {
    Canceled = 0,
    VTimer = 1,
    Mmio = 2,
    Hvc = 3,
    SysReg = 4,
    Other = 5,
}

const NAMES: [&str; 6] = ["canceled", "vtimer", "mmio", "hvc", "sysreg", "other"];

static COUNTS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

#[inline]
pub fn bump(kind: Kind) {
    COUNTS[kind as usize].fetch_add(1, Ordering::Relaxed);
}

/// Starts the reporter if asked for. Returns whether it did.
pub fn spawn_reporter_if_enabled() -> bool {
    if std::env::var("LIGHTER_EXIT_STATS").as_deref() != Ok("1") {
        return false;
    }
    std::thread::Builder::new()
        .name("exit-stats".into())
        .spawn(|| {
            let mut last = [0u64; 6];
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let mut line = String::new();
                let mut total = 0;
                for (i, name) in NAMES.iter().enumerate() {
                    let now = COUNTS[i].load(Ordering::Relaxed);
                    let delta = now.wrapping_sub(last[i]);
                    last[i] = now;
                    total += delta;
                    line.push_str(&format!(" {name}={}", delta / 2));
                }
                tracing::info!(per_second = total / 2, "exits{line}");
            }
        })
        .expect("spawning the exit reporter");
    true
}
