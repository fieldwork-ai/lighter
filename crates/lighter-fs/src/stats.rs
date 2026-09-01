//! Counting what the guest actually asks for.
//!
//! Filesystem tuning without this is guessing, and guessing about a shared
//! filesystem is unusually expensive: every plausible theory — the cache
//! timeouts, the worker count, the write size — costs a rebuild, a boot and a
//! benchmark run to disprove. A histogram of opcodes and the time spent in each
//! answers most of them in one run.
//!
//! Off unless `LIGHTER_FS_STATS` is set, and free when off: one relaxed atomic
//! load per request.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The opcodes worth naming in a report. Everything else lands in `other`.
const NAMED: [(u32, &str); 24] = [
    (1, "lookup"),
    (2, "forget"),
    (3, "getattr"),
    (4, "setattr"),
    (5, "readlink"),
    (6, "symlink"),
    (9, "mkdir"),
    (10, "unlink"),
    (11, "rmdir"),
    (12, "rename"),
    (13, "link"),
    (14, "open"),
    (15, "read"),
    (16, "write"),
    (17, "statfs"),
    (18, "release"),
    (20, "fsync"),
    (22, "getxattr"),
    (25, "flush"),
    (27, "opendir"),
    (28, "readdir"),
    (29, "releasedir"),
    (34, "access"),
    (35, "create"),
];

/// What to call an opcode in a log line.
pub fn name(opcode: u32) -> &'static str {
    NAMED
        .iter()
        .find(|(code, _)| *code == opcode)
        .map(|(_, name)| *name)
        .unwrap_or("other")
}

/// How many independent copies of the counters to keep.
///
/// Not paranoia: seventeen threads incrementing two atomics on one cache line,
/// a hundred thousand times, was measured making the workload three times
/// slower — so the instrumentation was reporting on a machine that only existed
/// while it was watching. A diagnostic that changes what it measures is worse
/// than none.
const LANES: usize = 8;

/// One lane of counters, on its own cache line.
#[repr(align(128))]
struct Lane {
    counts: [AtomicU64; NAMED.len() + 1],
    nanos: [AtomicU64; NAMED.len() + 1],
}

impl Lane {
    fn new() -> Lane {
        Lane {
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            nanos: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

/// How many requests were being served at the moment each new one arrived.
///
/// The histogram above answers "what is being asked and what does each answer
/// cost"; this answers the other half of a workload number — how many of those
/// answers overlap. A share can be slow two ways: each operation is slow, or
/// the guest sends one at a time and pays a round trip's latency serially. The
/// levers for those are disjoint, so tuning without this gauge is guessing at
/// which problem exists.
#[repr(align(128))]
struct Gauge {
    cur: AtomicU64,
    max: AtomicU64,
    sum: AtomicU64,
    arrivals: AtomicU64,
}

/// One counter per named opcode, plus a catch-all.
pub struct Stats {
    on: AtomicBool,
    lanes: [Lane; LANES],
    inflight: Gauge,
}

/// Which lane this thread writes to. Any stable spread will do; the report sums
/// them all.
fn lane() -> usize {
    thread_local! {
        static LANE: usize = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::thread::current().id().hash(&mut hasher);
            (hasher.finish() as usize) % LANES
        };
    }
    LANE.with(|lane| *lane)
}

impl Default for Stats {
    fn default() -> Stats {
        Stats::new()
    }
}

impl Stats {
    pub fn new() -> Stats {
        Stats {
            on: AtomicBool::new(std::env::var_os("LIGHTER_FS_STATS").is_some()),
            lanes: std::array::from_fn(|_| Lane::new()),
            inflight: Gauge {
                cur: AtomicU64::new(0),
                max: AtomicU64::new(0),
                sum: AtomicU64::new(0),
                arrivals: AtomicU64::new(0),
            },
        }
    }

    pub fn enabled(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    fn slot(opcode: u32) -> usize {
        // READDIRPLUS and RENAME2 are the same operation as their older
        // siblings from a tuning point of view, and splitting them makes the
        // report harder to read for no gain.
        let opcode = match opcode {
            44 => 28, // READDIRPLUS -> READDIR
            45 => 12, // RENAME2 -> RENAME
            42 => 2,  // BATCH_FORGET -> FORGET (one request, many forgets)
            _ => opcode,
        };
        match NAMED.iter().position(|(code, _)| *code == opcode) {
            Some(index) => index,
            None => NAMED.len(),
        }
    }

    /// A request is about to be served; counts itself among the concurrent.
    pub fn enter(&self) {
        let seen = self.inflight.cur.fetch_add(1, Ordering::Relaxed) + 1;
        self.inflight.max.fetch_max(seen, Ordering::Relaxed);
        self.inflight.sum.fetch_add(seen, Ordering::Relaxed);
        self.inflight.arrivals.fetch_add(1, Ordering::Relaxed);
    }

    pub fn exit(&self) {
        self.inflight.cur.fetch_sub(1, Ordering::Relaxed);
    }

    /// Records one served request.
    pub fn record(&self, opcode: u32, elapsed: std::time::Duration) {
        let slot = Stats::slot(opcode);
        let lane = &self.lanes[lane()];
        lane.counts[slot].fetch_add(1, Ordering::Relaxed);
        lane.nanos[slot].fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
    }

    fn total(&self, slot: usize) -> (u64, u64) {
        self.lanes.iter().fold((0, 0), |(count, nanos), lane| {
            (
                count + lane.counts[slot].load(Ordering::Relaxed),
                nanos + lane.nanos[slot].load(Ordering::Relaxed),
            )
        })
    }

    /// A one-line-per-opcode summary, most time first.
    pub fn report(&self) -> String {
        let mut rows: Vec<(&str, u64, u64)> = NAMED
            .iter()
            .enumerate()
            .map(|(index, (_, name))| {
                let (count, nanos) = self.total(index);
                (*name, count, nanos)
            })
            .chain(std::iter::once({
                let (count, nanos) = self.total(NAMED.len());
                ("other", count, nanos)
            }))
            .filter(|(_, count, _)| *count > 0)
            .collect();
        rows.sort_by_key(|(_, _, nanos)| std::cmp::Reverse(*nanos));

        let total: u64 = rows.iter().map(|(_, count, _)| count).sum();
        let arrivals = self.inflight.arrivals.load(Ordering::Relaxed);
        let mean = if arrivals > 0 {
            self.inflight.sum.load(Ordering::Relaxed) as f64 / arrivals as f64
        } else {
            0.0
        };
        let mut out = format!(
            "FSSTATS requests={total} inflight_mean={mean:.1} inflight_max={}\n",
            self.inflight.max.load(Ordering::Relaxed)
        );
        for (name, count, nanos) in rows {
            out.push_str(&format!(
                "FSSTATS {name:12} n={count:<9} total_ms={:<8} mean_us={:.1}\n",
                nanos / 1_000_000,
                nanos as f64 / count as f64 / 1000.0
            ));
        }
        out
    }

    pub fn reset(&self) {
        for lane in &self.lanes {
            for slot in 0..lane.counts.len() {
                lane.counts[slot].store(0, Ordering::Relaxed);
                lane.nanos[slot].store(0, Ordering::Relaxed);
            }
        }
        self.inflight.max.store(0, Ordering::Relaxed);
        self.inflight.sum.store(0, Ordering::Relaxed);
        self.inflight.arrivals.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn counts_and_times_are_grouped_by_opcode() {
        let stats = Stats::new();
        stats.record(1, Duration::from_micros(10));
        stats.record(1, Duration::from_micros(30));
        stats.record(15, Duration::from_micros(100));
        let report = stats.report();
        assert!(report.contains("requests=3"), "{report}");
        assert!(report.contains("lookup"), "{report}");
        // Read took longer in total, so it must be listed first: the whole
        // point is to see where the time went, not where the calls went.
        let read_at = report.find("read").unwrap();
        let lookup_at = report.find("lookup").unwrap();
        assert!(read_at < lookup_at, "{report}");
    }

    #[test]
    fn an_unnamed_opcode_is_still_counted() {
        let stats = Stats::new();
        stats.record(9999, Duration::from_micros(1));
        assert!(stats.report().contains("other"));
    }
}
