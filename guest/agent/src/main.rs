//! The lighter guest agent.
//!
//! Runs inside the VM and answers vsock connections from the host. Its whole
//! job is to be the guest end of a stream: the host's `~/.lighter/docker.sock`
//! becomes a vsock connection here, and this bridges it to the real
//! `/run/docker.sock` that dockerd is listening on.
//!
//! It is a separate binary rather than part of init because it must be
//! restartable without restarting PID 1, and separate from the VMM because it
//! is a Linux program.
//!
//! ```text
//!   docker CLI ──unix──▶ lighter ──vsock──▶ agent ──unix──▶ dockerd
//! ```

mod sockmap;
mod udp;
mod vsock;

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use vsock::VsockListener;

fn main() -> std::process::ExitCode {
    let mut port: u32 = 2375;
    let mut target: Option<String> = None;
    let mut echo = false;
    let mut control = false;
    let mut tcp_proxy: Option<u16> = None;
    let mut inbound: Option<u32> = None;
    let mut dns: Option<String> = None;
    let mut udp_proxy: Option<u16> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or(2375),
            "--to" => target = args.next(),
            // Answers connections itself instead of bridging. The vsock gate
            // uses it to prove the transport with nothing else installed.
            "--echo" => echo = true,
            // Answers the host's control commands. Small on purpose: the only
            // one that exists sets the clock, and the only reason it exists is
            // that a Mac that slept wakes with a guest whose clock did not.
            "--control" => control = true,
            // Takes the TCP connections netfilter redirects to it — every
            // connection from a container or the guest that would have left
            // through eth0 — and carries each to the host as a vsock stream.
            "--tcp-proxy" => tcp_proxy = args.next().and_then(|v| v.parse().ok()),
            // The other direction: a connection the Mac accepted on a
            // published port arrives as a vsock stream naming the port, and
            // is carried to the guest address Docker publishes on.
            "--inbound" => inbound = args.next().and_then(|v| v.parse().ok()),
            // Answers DNS on the given address by carrying each query to the
            // host over one vsock stream; the Mac's own resolver answers.
            "--dns" => dns = args.next(),
            // Takes the UDP datagrams netfilter redirects to it and carries
            // every flow to the host over one vsock stream (see udp.rs).
            "--udp-proxy" => udp_proxy = args.next().and_then(|v| v.parse().ok()),
            "--bpf-probe" => {
                sockmap::probe();
                return std::process::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("lighter-agent: unknown argument {other}");
                return std::process::ExitCode::from(2);
            }
        }
    }

    if let Some(port) = tcp_proxy {
        return serve_tcp_proxy(port);
    }
    if let Some(port) = inbound {
        return serve_inbound(port);
    }
    if let Some(port) = udp_proxy {
        let host = match vsock::connect(udp::UDP_PORT) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("lighter-agent: udp stream to host refused: {e}");
                return std::process::ExitCode::FAILURE;
            }
        };
        let _ = vsock::set_buffer(&host, STREAM_WINDOW);
        return match udp::serve(port, Fd(host)) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("lighter-agent: udp proxy: {e}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    if let Some(addr) = dns {
        return serve_dns(&addr);
    }
    if target.is_none() && !echo && !control {
        eprintln!("lighter-agent: one of --to <path>, --echo, --control or --tcp-proxy <port> is required");
        return std::process::ExitCode::from(2);
    }

    if control {
        std::thread::spawn(bound_container_cache);
    }

    let listener = match VsockListener::bind(port) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("lighter-agent: cannot bind vsock port {port}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // The line the gate waits for. Printed only once the socket is bound, so
    // seeing it means a connection now would be accepted rather than refused.
    println!("AGENT listening port={port}");

    loop {
        let stream = match listener.accept() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("lighter-agent: accept failed: {e}");
                continue;
            }
        };
        if control {
            let _ = vsock::set_buffer(&stream, STREAM_WINDOW);
        }
        let target = target.clone();
        std::thread::spawn(move || match (&target, control) {
            (Some(path), _) => bridge(stream, path),
            (None, true) => serve_control(stream),
            (None, false) => echo_back(stream),
        });
    }
}

/// Answers one control connection.
///
/// A line protocol, because the vocabulary is two words and a number and
/// anything more would be a serialization format to maintain. Each command
/// gets one line back so the host can tell "done" from "this build does not
/// know that word".
/// Gives the containers' page cache back once they have been idle a while.
///
/// From what an 8 GB Mac with a 4 GiB guest showed, in order. Left alone,
/// the cache fills the guest and macOS compresses the guest's pages while
/// reporting no pressure at all, and the install after a big one paid
/// fifteen percent faulting its memory back in. A bound on the cache — as
/// `memory.high`, at half of RAM or three quarters, or kept by reclaiming
/// above a line every second — cured that and cost more than it cured:
/// the first of three repetitions of every install took twice as long as
/// the third, and yarn ran a third slower throughout, because an install's
/// working set is the cache and any bound the working set crosses is paid
/// on every page. The host's compressor is left to the host-side policy,
/// which asks for a reclaim only under real distress; with that alone the
/// install after a big one paid three percent.
///
/// What remains is what OrbStack's footprint shows: back at two gigabytes
/// within a quarter minute of an install ending. Once the containers have
/// been idle ten seconds the cache is trimmed to a sixteenth of RAM,
/// coldest pages first (`memory.reclaim`), and free page reporting hands
/// the freed memory back. Idle is the containers' own CPU from their
/// cgroup, not the guest's — a guest installing against the share is three
/// quarters idle waiting on the host — and half a minute of it, because
/// three seconds was the pause between one install and the next and every
/// one started cold. dockerd makes the cgroup at the first container, which
/// can be any time, so this simply keeps looking.
fn bound_container_cache() {
    let total = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            m.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<u64>().ok())
        })
        .map(|kb| kb * 1024);
    let Some(total) = total else { return };
    let containers = "/sys/fs/cgroup/docker";
    // A bound on the containers' cache while they work, on guests with the
    // RAM for it: a quarter of RAM from eight gigabytes up, none below, and
    // `lighter.cachebound=<MiB>` on the command line to say otherwise (0
    // for none). An install's working set is a gigabyte or two and fits;
    // what the bound evicts is the stale cache of the builds before it,
    // which the suite's sequence otherwise grows to the whole of RAM —
    // a 16 GiB guest read 17 GB in Activity Monitor through an install
    // against OrbStack's 5.5, whose guest is sized dynamically. On the M1's
    // 4 GiB guests a bound at half or three quarters of RAM was measured to
    // cost an install a third, so small guests keep none. An eighth on the
    // 16 GiB guest was measured too: pnpm 4.2–7.9 s against 3.8–4.3, a
    // working set larger than two gigabytes paid on every page. It shipped
    // by accident for a morning (the UDP commit carried it), which is what
    // the pnpm regressions of 2026-09-04's memory runs were.
    //
    // Opt-in only (`lighter.cachebound=<MiB>`), never a default: on a daily
    // driver the containers' cgroup is every container the user runs at
    // once — Postgres, dev servers, brokers — and a shared ceiling there
    // is `memory.high` throttling the lot. A 24 GiB machine carrying the
    // benchmark's bound had 36 tasks in D state, a load of 80, every
    // health check failing and 782,575 throttle events before the bound
    // was lifted by hand. The benchmark asks for it on its own command
    // line; a machine that did not ask keeps its cache.
    // The per-CPU vmstat workers ran every second on eight vCPUs of an
    // idle guest — half of its kworker wakeups. Ten seconds is plenty for
    // counters nothing in here reads faster than the agent's own tick.
    let _ = std::fs::write("/proc/sys/vm/stat_interval", "10");
    let trims = cmdline_value("lighter.trim").is_none_or(|v| v != 0);
    let bound = cmdline_value("lighter.cachebound")
        .map(|mib| mib << 20)
        .unwrap_or(0);
    // dockerd makes the containers' cgroup at the first container, which
    // can be any time: the bound is written once it exists.
    let mut bounded = bound == 0;
    let always_fast = std::fs::read_to_string("/proc/cmdline")
        .map(|c| c.split_whitespace().any(|w| w == "lighter.reporting=fast"))
        .unwrap_or(false);
    if always_fast {
        set_reporting(100, 5);
    }
    // The engine's cgroup (init puts dockerd there): the image layers it
    // extracted, and whatever else it read, charged where a trim can reach.
    let engine = "/sys/fs/cgroup/engine";
    let mut idle_for = 0u32;
    let mut last = container_cpu_usec(containers);
    let mut memory_stream: Option<OwnedFd> = None;
    let mut last_offer: Option<[u8; 16]> = None;
    let mut cpu_last = guest_cpu_usec();
    let mut quiet_for = 0u32;
    // Quarter-second ticks: the offer to the host waits for two seconds of
    // idle, and a one-second tick put the first offer three seconds after
    // the last container stopped — past the moment anything looking at the
    // machine five seconds after its work would read.
    const TICKS_PER_SEC: u32 = 4;
    loop {
        // A quarter-second tick while anything is happening; one a second
        // once the guest has been idle ten seconds. Each tick is a timer
        // wakeup for a vCPU and, above eight gigabytes, a message to the
        // host: four a second of each on a machine doing nothing. The
        // counters step by four so the marks they are compared against
        // (the trims, the reporting rate at 25 s) fall where they did.
        let step = if idle_for >= 10 * TICKS_PER_SEC && quiet_for >= 10 * TICKS_PER_SEC { 4 } else { 1 };
        std::thread::sleep(std::time::Duration::from_millis(step as u64 * 1000 / TICKS_PER_SEC as u64));
        if !bounded && std::path::Path::new(containers).exists() {
            bounded = std::fs::write(format!("{containers}/memory.high"), bound.to_string()).is_ok();
            // The engine's cache (image layers) bounded too, at an eighth of
            // the containers': measured at a quarter of RAM for the
            // containers alone, the sequence still read ten gigabytes.
            let _ = std::fs::write(format!("{engine}/memory.high"), (bound / 8).to_string());
        }
        let now = container_cpu_usec(containers);
        let used = now.saturating_sub(last);
        last = now;
        idle_for = if used < step as u64 * 50_000 / TICKS_PER_SEC as u64 { idle_for + step } else { 0 };
        // Freed memory goes back at reporting's idle rate for a while after
        // a trim, and at its churn rate again once the containers work or
        // the burst is over (guest kernel patch 0019).
        // `lighter.reporting=fast` on the command line keeps reporting
        // hurried throughout, to measure what the churn of an install
        // costs against the footprint it holds while waiting to re-report.
        if (idle_for == 0 || idle_for == 25 * TICKS_PER_SEC) && !always_fast {
            set_reporting(2000, 9);
        }
        // Two seconds idle: offer the host what is free beyond a reserve,
        // through the balloon (`memory_guest` on the host side). What the
        // containers freed while they worked sits in the free lists in
        // file-sized pieces that reporting cannot return without a
        // compaction pass, and a pass costs the next command; the balloon
        // takes pages at host-page size from any free list. The offer is
        // withdrawn — the whole balloon asked back — the moment the
        // containers work again or free memory falls under the reserve.
        // The offer does not wait for the containers: free memory beyond the
        // reserve is free whatever they are doing, and taking it costs only a
        // fault on reuse. (Waiting for them was measured: a stopped
        // container's cgroup stays populated past its exit, its teardown
        // shows CPU for two seconds more, and the offer came four seconds
        // after the last case where anything looking at the machine looks
        // at five.) The release, though, is theirs: only work that is
        // running and short of memory asks the balloon back.
        // The offer waits for the guest as a whole to be idle — a second of
        // its CPU under a tenth of a core — not for the containers' cgroup,
        // which outlives a container's exit by seconds. An offer made while
        // work ran seesawed a 4 GiB guest: the work's next allocation
        // tripped the release, the next tick offered everything again, three
        // gigabytes moved every second and a half and copy-tree took twice
        // as long. The release stays with active work short of memory.
        let running = running_containers(containers);
        let active = idle_for == 0 && running > 0;
        let busy = guest_cpu_busy(&mut cpu_last);
        quiet_for = if busy < step as u64 * 100_000 / TICKS_PER_SEC as u64 { quiet_for + step } else { 0 };
        // Three seconds of it, not one: a second's pause between two
        // commands is the suite's gap between cases, and an offer made in
        // it had the next case deflating the balloon and faulting on every
        // page it touched — copy-tree 7.4 s against 5.2 on the M1. Three is
        // longer than the gap and inside the five seconds after work that a
        // footprint is read at.
        //
        // Guests of eight gigabytes and up. On a 4 GiB guest the reserve is
        // a large share of RAM and the balloon broke even against reporting
        // alone: the peak 600 MB better, the minute reading 500 MB worse,
        // one install a tenth slower; the 16 GiB guest gains on every
        // reading. Below the line reporting and the trims are the policy.
        if total >= 8 << 30 {
            offer_memory(&mut memory_stream, &mut last_offer, total, active, quiet_for >= 3 * TICKS_PER_SEC, running == 0);
        }
        // Two passes, five and ten seconds idle: the containers down to a
        // sixty-fourth of RAM (their warmest pages) and the engine to
        // almost nothing, since nothing it cached is a build's working
        // set; then the same again, and compaction again — what the first
        // trim freed was in file-sized pieces, and a second pass on a guest
        // that has been idle a while coalesces what the first missed. Five
        // because the suite pauses three seconds between installs, and a
        // trim between two would cost the second its cache; and because
        // OrbStack's footprint is back at its resting size within fifteen
        // seconds of an install. A sixteenth of RAM kept at the first pass
        // was measured: 0.4 GB back at five seconds, 1.4 at ten, and the
        // fifteen-second reading caught the second pass mid-drain.
        //
        // With no container running at all, three and eight seconds instead:
        // nothing is between commands then, and the cache is the last thing
        // the host is still holding for a guest that has stopped.
        let running = running_containers(containers);
        // Three and eight seconds whether or not a container is running:
        // five was chosen so a trim would not land between two installs
        // and cost the second its cache, and that cost was measured at
        // nothing (`lighter.trim=0`, three reps, level). Three is inside the
        // five seconds after work that a footprint is read at.
        let (first, second) = (3, 8);
        // `lighter.trim=0` keeps the caches: the A/B for what a trim costs
        // the next command.
        if !trims {
            continue;
        }
        let (floor, engine_floor) = match idle_for {
            x if x == first * TICKS_PER_SEC || x == second * TICKS_PER_SEC => (total / 64, 8 << 20),
            _ => continue,
        };
        for (cgroup, resting) in [(containers, floor), (engine, engine_floor)] {
            let current = std::fs::read_to_string(format!("{cgroup}/memory.current"))
                .ok()
                .and_then(|c| c.trim().parse::<u64>().ok())
                .unwrap_or(0);
            if current > resting {
                let _ = std::fs::write(format!("{cgroup}/memory.reclaim"), (current - resting).to_string());
            }
        }
        // What the trim freed is in pieces the size of the files that held
        // it; the balloon takes the bulk of it as it is, a host page at a
        // time, through the offer above. What it leaves — the reserve, an
        // eighth of RAM — is reporting's: hurried for a while and compacted
        // into reportable runs, as before the balloon. On a 4 GiB guest the
        // reserve alone read 600 MB more at a minute without this.
        set_reporting(100, 5);
        compact_until_reportable();
    }
}

/// The host port that takes the guest's memory offer.
const MEMORY_PORT: u32 = 2381;

/// Sixteen bytes to the host: the free memory beyond a reserve of an
/// eighth of RAM (MiB; zero holds the balloon where it is), available
/// and free (MiB), and a release flag that asks the whole balloon back.
/// Reconnects on the next tick if the stream is gone; the host treats a
/// closed stream as a release.
/// Containers with a process in them: one child cgroup each under the
/// containers' cgroup.
fn running_containers(containers: &str) -> usize {
    std::fs::read_dir(containers)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter(|e| {
                    std::fs::read_to_string(e.path().join("cgroup.procs"))
                        .map(|p| !p.trim().is_empty())
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

/// Busy CPU time of the whole guest since the last call, in microseconds
/// of core (user, system and interrupt time from `/proc/stat`).
fn guest_cpu_busy(last: &mut u64) -> u64 {
    let now = guest_cpu_usec();
    let busy = now.saturating_sub(*last);
    *last = now;
    busy
}

fn guest_cpu_usec() -> u64 {
    let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let Some(line) = stat.lines().next() else { return 0 };
    let fields: Vec<u64> = line.split_whitespace().skip(1).filter_map(|f| f.parse().ok()).collect();
    // user nice system idle iowait irq softirq steal: everything but idle
    // and iowait, in jiffies at USER_HZ (100), to microseconds.
    let busy: u64 = fields.iter().enumerate().filter(|(i, _)| *i != 3 && *i != 4).map(|(_, v)| *v).sum();
    busy * 10_000
}

/// Sends only what changed: the host keeps the last offer until the next,
/// so an idle guest repeating itself four times a second was four host
/// wakeups a second for nothing. A reconnect resends.
fn offer_memory(
    stream: &mut Option<OwnedFd>,
    last: &mut Option<[u8; 16]>,
    total: u64,
    active: bool,
    quiet: bool,
    nothing_runs: bool,
) {
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |name: &str| -> u64 {
        meminfo
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|kb| kb.parse::<u64>().ok())
            .unwrap_or(0)
            >> 10
    };
    let (avail, free) = (field("MemAvailable:"), field("MemFree:"));
    // An eighth of RAM while containers run — a command starting draws on
    // it while the balloon deflates — and a sixteenth when nothing runs at
    // all: a 4 GiB guest with no container kept 512 MiB free that
    // reporting could not return in runs, 600 MB at a minute against the
    // record. A thirty-second was tried: 128 MiB left the container that
    // materializes the next case's tree without room to start.
    let reserve = (total >> 20) / if nothing_runs { 16 } else { 8 };
    // Release is its own word: an offer of zero means "nothing more", and
    // the balloon holds what it has. Said as one number, the guest asked
    // for everything back each time inflation dipped it under its line,
    // and the target flapped between fourteen gigabytes and none.
    //
    // And the balloon is asked back when memory is wanted, not when a
    // container merely does something: a daily driver's idle Postgres
    // answering a query would otherwise deflate gigabytes and take them
    // again two seconds later, every time. Work draws on the reserve —
    // an eighth of RAM — and when half of it is gone the whole balloon
    // comes back at a few gigabytes a second; deflate-on-OOM stands
    // behind that.
    // Not on free memory alone: inflation toward a target computed a tick
    // ago dips free memory under the line for a moment, and a release on
    // that dip alternated with the next offer, every tick.
    // Free memory, never available memory: MemAvailable counts the page
    // cache the kernel could reclaim, and a target that counted it had the
    // balloon driver reclaiming the cache to fill itself — on a 4 GiB
    // guest every ripgrep repetition ran cold, copy-tree took four times
    // as long and pnpm twice. The cache is the trims' to give up, when the
    // containers have been idle; the balloon takes only what is already
    // free.
    let release = active && free < reserve / 2;
    let spare = if !release && quiet && free > reserve + reserve / 4 {
        free - reserve
    } else {
        0
    };
    if stream.is_none() {
        *stream = vsock::connect(MEMORY_PORT).ok();
        *last = None;
    }
    let Some(fd) = stream.as_ref() else { return };
    let mut bytes = [0u8; 16];
    // Available and free are rounded to 16 MiB: they drift by a page or two
    // on an idle guest and would defeat the comparison.
    let coarse = |v: u64| (v & !15) as u32;
    for (i, v) in [spare as u32, coarse(avail), coarse(free), u32::from(release)].iter().enumerate() {
        bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    if *last == Some(bytes) {
        return;
    }
    // SAFETY: a plain write of a stack buffer to a descriptor we own.
    let n = unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), 16) };
    if n != 16 {
        *stream = None;
        *last = None;
    } else {
        *last = Some(bytes);
    }
}

/// A `key=<n>` on the kernel command line, if given.
fn cmdline_value(key: &str) -> Option<u64> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").ok()?;
    cmdline
        .split_whitespace()
        .find_map(|w| w.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .and_then(|v| v.parse().ok())
}

/// The kernel's delay before a free page reporting cycle (patch 0019), the
/// smallest order it reports, and how hard the background compactor works
/// meanwhile: hurried reporting goes with full-strength compaction, the
/// churn setting with the kernel's default.
fn set_reporting(delay_ms: u32, order: u32) {
    let _ = std::fs::write("/sys/module/page_reporting/parameters/page_reporting_delay_ms", delay_ms.to_string());
    let _ = std::fs::write("/sys/module/page_reporting/parameters/page_reporting_order", order.to_string());
    let proactiveness = if delay_ms < 2000 { "100" } else { "20" };
    let _ = std::fs::write("/proc/sys/vm/compaction_proactiveness", proactiveness);
}

/// Runs compaction until little free memory is left below the reporting
/// order (two megabytes), a bounded number of passes.
fn compact_until_reportable() {
    for _ in 0..12 {
        let _ = std::fs::write("/proc/sys/vm/compact_memory", "1");
        if free_below_reporting_order() < 64 << 20 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Bytes free in orders below nine, from /proc/buddyinfo, across all zones.
fn free_below_reporting_order() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/buddyinfo") else { return 0 };
    let mut total = 0u64;
    for line in text.lines() {
        // "Node 0, zone   Normal  a b c ..." — the counts start after "zone <name>".
        let Some(pos) = line.find("zone") else { continue };
        let counts: Vec<u64> = line[pos..]
            .split_whitespace()
            .skip(2)
            .filter_map(|w| w.parse().ok())
            .collect();
        for (order, n) in counts.iter().enumerate().take(9) {
            total += n * (4096u64 << order);
        }
    }
    total
}

/// CPU time the containers have used, in microseconds, from their cgroup.
fn container_cpu_usec(cgroup: &str) -> u64 {
    std::fs::read_to_string(format!("{cgroup}/cpu.stat"))
        .ok()
        .and_then(|stat| {
            stat.lines()
                .find(|l| l.starts_with("usage_usec "))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}

/// The host's vsock port for outbound streams.
const STREAM_PORT: u32 = 2377;

/// The kernel-side join for streams, if this kernel and this boot allow
/// it (`lighter.nosockmap` on the command line keeps the copying path).
fn joiner() -> Option<&'static sockmap::Joiner> {
    static JOINER: std::sync::OnceLock<Option<sockmap::Joiner>> = std::sync::OnceLock::new();
    JOINER
        .get_or_init(|| {
            let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
            if cmdline.split_whitespace().any(|w| w == "lighter.nosockmap") {
                return None;
            }
            match sockmap::Joiner::new() {
                Ok(j) => {
                    println!("AGENT sockmap=on");
                    Some(j)
                }
                Err(e) => {
                    eprintln!("lighter-agent: streams copy in this process: {e}");
                    None
                }
            }
        })
        .as_ref()
}

/// Two sockets joined in the kernel until both are done: the kernel moves
/// the bytes and, when either end's stream ends, shuts the other down for
/// sending behind the last of them (guest kernel patch 0016). This thread
/// waits for the first byte, joins, and then only waits for the hangup on
/// both. Returns the sockets when the stream is one the join cannot carry,
/// for the copying path.
fn joined(
    joiner: &sockmap::Joiner,
    tcp: std::net::TcpStream,
    host_read: Fd,
    host_write: Fd,
) -> Result<(), (std::net::TcpStream, Fd, Fd)> {
    let (t, h) = (tcp.as_raw_fd(), host_write.0.as_raw_fd());
    // Nothing is joined until a byte exists to carry: a connection that is
    // opened and closed (a probe, a health check) costs the join's ten
    // syscalls nothing, and one that is opened and left costs no psock.
    let mut first = [
        libc::pollfd { fd: t, events: libc::POLLIN | libc::POLLRDHUP, revents: 0 },
        libc::pollfd { fd: h, events: libc::POLLIN | libc::POLLRDHUP, revents: 0 },
    ];
    loop {
        // SAFETY: two live descriptors in a pollfd array.
        let n = unsafe { libc::poll(first.as_mut_ptr(), 2, -1) };
        if n >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            break;
        }
    }
    let ended = libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
    let data = (first[0].revents | first[1].revents) & libc::POLLIN != 0;
    if first[1].revents & ended != 0 {
        // The host's end of a stream that ended before the join never
        // reaches the kernel's marker (it is queued only for a socket
        // already joined). With nothing to carry the stream is simply over;
        // with bytes queued the copying path carries them.
        if !data {
            return Ok(());
        }
        return Err((tcp, host_read, host_write));
    }
    let slots = match joiner.join(t, h) {
        Ok(slots) => slots,
        Err(_) => return Err((tcp, host_read, host_write)),
    };
    // A socket reports HUP once both its directions are shut: its peer's
    // end seen, and its own sent by the kernel behind the redirected bytes.
    // HUP on both means neither backlog holds anything. An error on either
    // is an abort, and the other side is closed with whatever it has.
    let mut fds = [
        libc::pollfd { fd: t, events: libc::POLLRDHUP, revents: 0 },
        libc::pollfd { fd: h, events: libc::POLLRDHUP, revents: 0 },
    ];
    let mut hups = 0;
    while hups < 2 {
        // SAFETY: two descriptors (or -1 for one already done) in a pollfd array.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        let mut aborted = false;
        for p in fds.iter_mut() {
            if p.fd < 0 {
                continue;
            }
            if p.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                aborted = true;
            } else if p.revents & libc::POLLHUP != 0 {
                hups += 1;
                p.fd = -1;
            } else if p.revents & libc::POLLRDHUP != 0 {
                // Level-triggered; the half-close is noted, the HUP is what
                // is waited for now.
                p.events = 0;
            }
        }
        if aborted {
            break;
        }
    }
    // Closed before the slots go back: a closed socket has left the maps.
    drop(tcp);
    drop(host_read);
    drop(host_write);
    joiner.release(slots);
    Ok(())
}


/// The credit window a stream advertises: a millisecond of a fast link.
const STREAM_WINDOW: u64 = 8 << 20;

/// Where a redirected connection was really going, from conntrack.
///
/// netfilter's REDIRECT rewrote the destination to this machine and kept the
/// original in the connection's conntrack entry; SO_ORIGINAL_DST reads it
/// back. IPv4 first, then IPv6, because a socket accepted on a dual-stack
/// listener answers one or the other and nothing says which in advance.
fn original_destination(fd: libc::c_int) -> Option<(std::net::IpAddr, u16)> {
    const SOL_IP: libc::c_int = 0;
    const SOL_IPV6: libc::c_int = 41;
    const SO_ORIGINAL_DST: libc::c_int = 80;
    let mut v4: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_in>() as libc::socklen_t;
    // SAFETY: the buffer is a sockaddr_in and `len` its size.
    if unsafe { libc::getsockopt(fd, SOL_IP, SO_ORIGINAL_DST, std::ptr::addr_of_mut!(v4).cast(), &mut len) } == 0 {
        let ip = std::net::Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr));
        return Some((ip.into(), u16::from_be(v4.sin_port)));
    }
    let mut v6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_in6>() as libc::socklen_t;
    // SAFETY: the buffer is a sockaddr_in6 and `len` its size.
    if unsafe { libc::getsockopt(fd, SOL_IPV6, SO_ORIGINAL_DST, std::ptr::addr_of_mut!(v6).cast(), &mut len) } == 0 {
        let ip = std::net::Ipv6Addr::from(v6.sin6_addr.s6_addr);
        return Some((ip.into(), u16::from_be(v6.sin6_port)));
    }
    None
}

/// Accepts the connections netfilter redirects here and carries each to the
/// host as one vsock stream, the destination in a fixed header first.
///
/// TCP as streams, not packets: the guest's own kernel terminates the
/// container's connection and the host's kernel originates the real one, so
/// the only thing crossing the boundary is bytes, with no stack in between
/// to get wrong. The header is what the host needs to dial: a family byte,
/// sixteen bytes of address (IPv4 in the first four), and the port.
fn serve_tcp_proxy(port: u16) -> std::process::ExitCode {
    let listener = match std::net::TcpListener::bind((std::net::Ipv6Addr::UNSPECIFIED, port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("lighter-agent: cannot bind tcp port {port}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    println!("AGENT tcp-proxy port={port}");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        std::thread::spawn(move || forward_outbound(stream));
    }
    std::process::ExitCode::SUCCESS
}

static HOST_TIMEOUTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static HOST_LOST_REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A host that stops answering vsock connects is, from inside, a stall with
/// no visible cause: the control channel that would carry a diagnosis rides
/// the same device. So after a few consecutive timeouts the guest reports
/// its own state the one way that still reaches the host — the console:
/// sysrq's memory summary, blocked tasks, and every task's stack, once.
fn host_lost() {
    use std::sync::atomic::Ordering;
    if HOST_TIMEOUTS.fetch_add(1, Ordering::Relaxed) + 1 < 3 {
        return;
    }
    if HOST_LOST_REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!("lighter-agent: the host has stopped answering; dumping the guest's state to the console");
    for key in ["m", "w", "t"] {
        let _ = std::fs::write("/proc/sysrq-trigger", key);
    }
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines().take(8) {
            eprintln!("lighter-agent: {line}");
        }
    }
}

fn forward_outbound(tcp: std::net::TcpStream) {
    let Some((ip, port)) = original_destination(tcp.as_raw_fd()) else {
        return;
    };
    let host = match vsock::connect(STREAM_PORT) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("lighter-agent: stream to host refused: {e}");
            if e.raw_os_error() == Some(libc::ETIMEDOUT) {
                host_lost();
            }
            return;
        }
    };
    HOST_TIMEOUTS.store(0, std::sync::atomic::Ordering::Relaxed);
    let _ = vsock::set_buffer(&host, STREAM_WINDOW);
    let mut header = [0u8; 19];
    match ip {
        std::net::IpAddr::V4(a) => {
            header[0] = 4;
            header[1..5].copy_from_slice(&a.octets());
        }
        std::net::IpAddr::V6(a) => {
            header[0] = 6;
            header[1..17].copy_from_slice(&a.octets());
        }
    }
    header[17..19].copy_from_slice(&port.to_be_bytes());
    let mut host_write = Fd(host);
    let Ok(host_read) = host_write.try_clone() else { return };
    if host_write.write_all(&header).is_err() {
        return;
    }
    let _ = tcp.set_nodelay(true);
    let (tcp, host_read, mut host_write) = match joiner() {
        Some(j) => match joined(j, tcp, host_read, host_write) {
            Ok(()) => return,
            Err(back) => back,
        },
        None => (tcp, host_read, host_write),
    };
    let mut host_read = host_read;
    let Ok(mut tcp_read) = tcp.try_clone() else { return };
    let mut tcp_write = tcp;

    // container -> host, spliced through the kernel where it can be
    let outbound = std::thread::spawn(move || {
        let (tcp_fd, host_fd) = (tcp_read.as_raw_fd(), host_write.0.as_raw_fd());
        splice_copy(&tcp_fd, &host_fd, || copy(&mut tcp_read, &mut host_write));
        // SAFETY: a live descriptor; shutdown of the write half only, so the
        // reply direction stays open.
        unsafe { libc::shutdown(host_fd, libc::SHUT_WR) };
    });
    // host -> container
    copy(&mut host_read, &mut tcp_write);
    let _ = tcp_write.shutdown(std::net::Shutdown::Write);
    let _ = outbound.join();
}

/// The host's vsock port for DNS.
const DNS_PORT: u32 = 2379;

/// DNS for the guest and its containers, answered by the Mac's resolver.
///
/// One UDP socket here, one vsock stream to the host, queries multiplexed
/// on it as `[len u16][client id u16][query]`, replies the same way back.
/// The client id is ours, not DNS's own, because two clients may use the
/// same transaction id at once. The host forwards each query to the
/// resolver macOS is configured with — a VPN's, when one is up — so a
/// container sees the names the Mac sees.
fn serve_dns(addr: &str) -> std::process::ExitCode {
    let socket = match std::net::UdpSocket::bind((addr, 53)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lighter-agent: cannot bind dns on {addr}:53: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let host = match vsock::connect(DNS_PORT) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("lighter-agent: dns stream to host refused: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let mut host_write = Fd(host);
    let Ok(mut host_read) = host_write.try_clone() else {
        return std::process::ExitCode::FAILURE;
    };
    println!("AGENT dns addr={addr}");
    // Who asked, by our id, so the reply finds its way back. Bounded and
    // overwritten in order: a reply for a forgotten query is dropped.
    let clients: std::sync::Arc<std::sync::Mutex<[Option<(std::net::SocketAddr, u16)>; 4096]>> =
        std::sync::Arc::new(std::sync::Mutex::new([None; 4096]));
    let reply_socket = match socket.try_clone() {
        Ok(s) => s,
        Err(_) => return std::process::ExitCode::FAILURE,
    };
    let reply_clients = clients.clone();
    std::thread::spawn(move || {
        let mut header = [0u8; 4];
        loop {
            if host_read.read_exact(&mut header).is_err() {
                return;
            }
            let len = u16::from_be_bytes([header[0], header[1]]) as usize;
            let id = u16::from_be_bytes([header[2], header[3]]) as usize;
            let mut reply = vec![0u8; len];
            if host_read.read_exact(&mut reply).is_err() {
                return;
            }
            let client = reply_clients.lock().expect("dns clients poisoned")[id % 4096];
            if let Some((client, original)) = client {
                // The reply carries our id; the client expects its own.
                reply[0..2].copy_from_slice(&original.to_be_bytes());
                let _ = reply_socket.send_to(&reply, client);
            }
        }
    });
    let mut buf = [0u8; 4096];
    let mut next: u16 = 0;
    loop {
        let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
        if n < 12 || n > 4000 {
            continue;
        }
        let id = next;
        next = next.wrapping_add(1) % 4096;
        let original = u16::from_be_bytes([buf[0], buf[1]]);
        clients.lock().expect("dns clients poisoned")[id as usize] = Some((from, original));
        let mut frame = Vec::with_capacity(4 + n);
        frame.extend_from_slice(&(n as u16).to_be_bytes());
        frame.extend_from_slice(&id.to_be_bytes());
        frame.extend_from_slice(&buf[..n]);
        if host_write.write_all(&frame).is_err() {
            eprintln!("lighter-agent: dns stream to host closed");
            return std::process::ExitCode::FAILURE;
        }
    }
}

/// The address Docker publishes ports on inside this guest: eth0's, which
/// gvproxy leases by DHCP. Loopback would not do — with no userland proxy a
/// published port is a DNAT rule, and Docker's rule exempts loopback.
const PUBLISHED_ADDR: std::net::Ipv4Addr = std::net::Ipv4Addr::new(192, 168, 127, 2);

/// Answers the host's inbound streams: two bytes of port, then bytes both
/// ways to whatever Docker has on that port.
fn serve_inbound(port: u32) -> std::process::ExitCode {
    let listener = match VsockListener::bind(port) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("lighter-agent: cannot bind vsock port {port}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    println!("AGENT inbound port={port}");
    loop {
        let Ok(stream) = listener.accept() else { continue };
        let _ = vsock::set_buffer(&stream, STREAM_WINDOW);
        std::thread::spawn(move || forward_inbound(stream));
    }
}

fn forward_inbound(host: OwnedFd) {
    let mut host_read = Fd(host);
    let Ok(host_write) = host_read.try_clone() else { return };
    let mut header = [0u8; 2];
    if host_read.read_exact(&mut header).is_err() {
        return;
    }
    let port = u16::from_be_bytes(header);
    let tcp = match std::net::TcpStream::connect((PUBLISHED_ADDR, port)) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("lighter-agent: inbound to {PUBLISHED_ADDR}:{port} refused: {e}");
            return;
        }
    };
    let _ = tcp.set_nodelay(true);
    let (tcp, host_read, host_write) = match joiner() {
        Some(j) => match joined(j, tcp, host_read, host_write) {
            Ok(()) => return,
            Err(back) => back,
        },
        None => (tcp, host_read, host_write),
    };
    let (mut host_read, mut host_write) = (host_read, host_write);
    let Ok(mut tcp_read) = tcp.try_clone() else { return };
    let mut tcp_write = tcp;
    // host -> container
    let inbound = std::thread::spawn(move || {
        copy(&mut host_read, &mut tcp_write);
        let _ = tcp_write.shutdown(std::net::Shutdown::Write);
    });
    // container -> host, spliced through the kernel where it can be
    let (tcp_fd, host_fd) = (tcp_read.as_raw_fd(), host_write.0.as_raw_fd());
    splice_copy(&tcp_fd, &host_fd, || copy(&mut tcp_read, &mut host_write));
    // SAFETY: a live descriptor; shutdown of the write half only.
    unsafe { libc::shutdown(host_fd, libc::SHUT_WR) };
    let _ = inbound.join();
}

fn serve_control(stream: OwnedFd) {
    let mut reader = Fd(stream);
    let mut writer = match reader.try_clone() {
        Ok(fd) => fd,
        Err(_) => return,
    };

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buffer.extend_from_slice(&chunk[..read]);
        while let Some(end) = buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line[..end]).trim().to_string();
            // The two verbs that move bytes rather than words, for measuring
            // the channel itself: `blast N` writes N bytes as fast as the
            // socket takes them, `sink N` reads N bytes and then says so.
            // Raw bytes on the line protocol's own connection, so what is
            // measured is exactly what a stream over this device costs.
            let mut words = line.split_whitespace();
            match (words.next(), words.next().and_then(|n| n.parse::<u64>().ok())) {
                (Some("blast"), Some(mut left)) => {
                    let chunk = vec![0x5au8; 256 * 1024];
                    while left > 0 {
                        let take = (chunk.len() as u64).min(left) as usize;
                        if writer.write_all(&chunk[..take]).is_err() {
                            return;
                        }
                        left -= take as u64;
                    }
                    continue;
                }
                (Some("sink"), Some(mut left)) => {
                    // Whatever followed the line in the same read is data.
                    let have = buffer.len().min(left as usize);
                    buffer.drain(..have);
                    left -= have as u64;
                    let mut big = vec![0u8; 256 * 1024];
                    while left > 0 {
                        let want = big.len().min(left as usize);
                        match reader.read(&mut big[..want]) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => left -= n as u64,
                        }
                    }
                    if writer.write_all(b"sunk\n").is_err() {
                        return;
                    }
                    continue;
                }
                _ => {}
            }
            let reply = handle_control(&line);
            if writer.write_all(reply.as_bytes()).is_err() {
                return;
            }
        }
        if buffer.len() > 4096 {
            // Nothing legitimate is this long; a peer sending it is confused.
            return;
        }
    }
}

fn handle_control(line: &str) -> String {
    let mut words = line.split_whitespace();
    match (words.next(), words.next()) {
        (Some("ping"), _) => "pong\n".into(),
        // The host is compressing our pages: give back that much page cache,
        // coldest first, from the cgroup every container lives in. The
        // kernel frees it in bulk and free page reporting returns it to the
        // host in runs it can take, which the balloon's scattered 4 KiB
        // pages never were.
        (Some("reclaim"), Some(amount)) => match amount.parse::<u64>() {
            Err(_) => "error bad amount\n".into(),
            Ok(mib) => match std::fs::write("/sys/fs/cgroup/docker/memory.reclaim", format!("{mib}M")) {
                Ok(()) => "ok\n".into(),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => "partial\n".into(),
                Err(e) => format!("error {e}\n"),
            },
        },
        // Diagnostics that touch no disk: procfs, sysfs and the kernel log
        // stay readable when a block device has wedged.
        (Some("read"), Some(path)) => match std::fs::read(path) {
            Ok(bytes) => {
                let mut out = String::from_utf8_lossy(&bytes[..bytes.len().min(1 << 16)]).into_owned();
                out.push_str("\n--end--\n");
                out
            }
            Err(e) => format!("error {e}\n--end--\n"),
        },
        // Diagnostics only: a shell command, output and exit status back.
        (Some("sh"), Some(_)) => {
            let command = line.trim_start_matches("sh").trim();
            match std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(command)
                .output()
            {
                Ok(out) => format!(
                    "{}{}\nexit={}\n--end--\n",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr),
                    out.status.code().unwrap_or(-1)
                ),
                Err(e) => format!("error {e}\n--end--\n"),
            }
        }
        // Diagnostics only: bytes of guest-physical memory, through the
        // direct map that /proc/kcore exposes.
        (Some("peek"), Some(addr)) => {
            let len = words.next().and_then(|w| w.parse().ok()).unwrap_or(16usize);
            let addr = u64::from_str_radix(addr.trim_start_matches("0x"), 16).unwrap_or(0);
            match peek_physical(addr, len.min(4096)) {
                Ok(bytes) => {
                    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
                    format!("{}\n--end--\n", hex.join(" "))
                }
                Err(e) => format!("error {e}\n--end--\n"),
            }
        }
        (Some("kmsg"), _) => match read_kmsg() {
            Ok(text) => format!("{text}\n--end--\n"),
            Err(e) => format!("error {e}\n--end--\n"),
        },
        (Some("time"), Some(seconds)) => match seconds.parse::<i64>() {
            Ok(epoch) => match set_clock(epoch) {
                Ok(()) => "ok\n".into(),
                Err(e) => format!("error {e}\n"),
            },
            Err(_) => "error not-a-number\n".into(),
        },
        _ => "error unknown\n".into(),
    }
}

/// Reads guest-physical memory through `/proc/kcore`, whose largest LOAD
/// segment is the kernel's direct map of System RAM, laid out from the
/// first RAM address in `/proc/iomem`.
fn peek_physical(pa: u64, len: usize) -> Result<Vec<u8>, std::io::Error> {
    use std::io::{Seek, SeekFrom};
    let iomem = std::fs::read_to_string("/proc/iomem")?;
    let ram_start = iomem
        .lines()
        .find(|l| l.contains("System RAM"))
        .and_then(|l| l.trim().split('-').next())
        .and_then(|s| u64::from_str_radix(s.trim(), 16).ok())
        .ok_or_else(|| std::io::Error::other("no System RAM in /proc/iomem"))?;
    let mut kcore = std::fs::File::open("/proc/kcore")?;
    let mut ehdr = [0u8; 64];
    kcore.read_exact(&mut ehdr)?;
    let phoff = u64::from_le_bytes(ehdr[32..40].try_into().unwrap());
    let phentsize = u16::from_le_bytes(ehdr[54..56].try_into().unwrap()) as u64;
    let phnum = u16::from_le_bytes(ehdr[56..58].try_into().unwrap()) as u64;
    // The direct map of System RAM: the LOAD segment whose physical address
    // is the start of RAM, and the largest such (the kernel image is a
    // smaller one at the same physical address).
    let mut best: Option<(u64, u64, u64)> = None; // (offset, vaddr, memsz)
    for i in 0..phnum {
        kcore.seek(SeekFrom::Start(phoff + i * phentsize))?;
        let mut ph = [0u8; 56];
        kcore.read_exact(&mut ph)?;
        let p_type = u32::from_le_bytes(ph[0..4].try_into().unwrap());
        if p_type != 1 {
            continue;
        }
        let offset = u64::from_le_bytes(ph[8..16].try_into().unwrap());
        let vaddr = u64::from_le_bytes(ph[16..24].try_into().unwrap());
        let paddr = u64::from_le_bytes(ph[24..32].try_into().unwrap());
        let memsz = u64::from_le_bytes(ph[40..48].try_into().unwrap());
        if paddr != ram_start {
            continue;
        }
        if best.map_or(true, |(_, _, m)| memsz > m) {
            best = Some((offset, vaddr, memsz));
        }
    }
    let (offset, _vaddr, memsz) = best.ok_or_else(|| std::io::Error::other("no LOAD segment"))?;
    if pa < ram_start || pa - ram_start + len as u64 > memsz {
        return Err(std::io::Error::other("address outside System RAM"));
    }
    kcore.seek(SeekFrom::Start(offset + (pa - ram_start)))?;
    let mut buf = vec![0u8; len];
    kcore.read_exact(&mut buf)?;
    Ok(buf)
}

/// The kernel log, read record by record from `/dev/kmsg` until it has no
/// more, without blocking.
fn read_kmsg() -> Result<String, std::io::Error> {
    use std::os::fd::FromRawFd;
    let path = std::ffi::CString::new("/dev/kmsg").expect("static path");
    // SAFETY: a valid C string; the descriptor is owned below.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: just opened, owned by nothing else.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut out = String::new();
    let mut buf = [0u8; 8192];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.push_str(&String::from_utf8_lossy(&buf[..n]));
                if out.len() > (1 << 18) {
                    out.drain(..out.len() - (1 << 17));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.raw_os_error() == Some(libc::EPIPE) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// Sets the guest's wall clock.
///
/// The machine has no real-time clock, so the guest's idea of the time comes
/// from the host at boot and then drifts — and after the Mac sleeps, it does
/// not so much drift as stop. Everything that checks a certificate breaks, and
/// the error names the certificate rather than the clock.
fn set_clock(epoch: i64) -> Result<(), std::io::Error> {
    let tv = libc::timeval {
        tv_sec: epoch as libc::time_t,
        tv_usec: 0,
    };
    // SAFETY: a correctly-shaped timeval and a null timezone, which is the
    // documented way to leave the timezone alone.
    let rc = unsafe { libc::settimeofday(&tv, std::ptr::null()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Copies between the vsock connection and a unix socket until either ends.
fn bridge(guest_side: OwnedFd, path: &str) {
    let upstream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(e) => {
            // The errno is the whole diagnosis here: "no such file" means the
            // daemon has not created its socket yet, "connection refused"
            // means it died after creating one, and "permission denied" means
            // something quite different again. Reporting only the path sends
            // you looking in the wrong place.
            eprintln!("lighter-agent: cannot reach {path}: {e}");
            return;
        }
    };
    let Ok(mut upstream_read) = upstream.try_clone() else {
        return;
    };
    let mut upstream_write = upstream;

    // Two threads rather than poll(): the whole point of this process is to be
    // simple enough to trust, and a stream copy in each direction is the
    // simplest thing that cannot deadlock on a half-full pipe.
    //
    // The two directions are named rather than inferred. They were once both
    // written as "guest to upstream" — which connects, forwards the request,
    // and then hangs forever waiting for a reply nobody is carrying back.
    let mut guest_read = Fd(guest_side);
    let mut guest_write = match guest_read.try_clone() {
        Ok(fd) => fd,
        Err(_) => return,
    };

    // guest -> dockerd
    let request = std::thread::spawn(move || {
        copy(&mut guest_read, &mut upstream_write);
        // Let dockerd see the end of the request rather than waiting on a
        // connection that will send no more.
        let _ = upstream_write.shutdown(std::net::Shutdown::Write);
    });

    // dockerd -> guest
    copy(&mut upstream_read, &mut guest_write);
    // The daemon has said everything it will say. The host client is owed the
    // end of the response even though its own stdin may stay open forever —
    // `docker exec` with a terminal for stdin does exactly that, and without
    // this half-close it hangs on a reply that finished long ago. SHUT_WR
    // rather than dropping the fd: the request direction may still be
    // draining, and it ends on its own terms.
    // SAFETY: a live descriptor; shutdown of the write half only.
    unsafe { libc::shutdown(guest_write.0.as_raw_fd(), libc::SHUT_WR) };

    let _ = request.join();
}

/// Sends back whatever arrives, so the gate can prove the round trip.
fn echo_back(stream: OwnedFd) {
    let mut fd = Fd(stream);
    let mut buf = [0u8; 4096];
    loop {
        match fd.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if fd.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
        }
    }
}

/// Moves a TCP socket's bytes to another descriptor without passing them
/// through this process: `splice` from the socket into a pipe and from the
/// pipe onward, both inside the kernel. The container's side of a stream is
/// TCP, which supports it; the vsock side takes the pipe's pages through
/// `sendmsg`. Falls back to [`copy`] on a kernel or socket that refuses,
/// which is how the other direction still moves — a vsock socket cannot be
/// spliced *from*.
fn splice_copy(from: &impl AsRawFd, to: &impl AsRawFd, fallback: impl FnOnce()) {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: a two-int array for pipe2 to fill.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return fallback();
    }
    // SAFETY: fresh descriptors we own.
    let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // The pipe's capacity bounds each splice; a megabyte keeps the two
    // calls per chunk from being the cost.
    // SAFETY: F_SETPIPE_SZ with an int argument.
    unsafe { libc::fcntl(wr.as_raw_fd(), libc::F_SETPIPE_SZ, 4 << 20) };
    const CHUNK: usize = 4 << 20;
    let mut first = true;
    loop {
        // SAFETY: descriptors are live; null offsets for sockets and pipes.
        let n = unsafe {
            libc::splice(from.as_raw_fd(), std::ptr::null_mut(), wr.as_raw_fd(), std::ptr::null_mut(), CHUNK, libc::SPLICE_F_MOVE)
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if first && e.raw_os_error() == Some(libc::EINVAL) {
                return fallback();
            }
            break;
        }
        if n == 0 {
            break;
        }
        first = false;
        let mut left = n as usize;
        while left > 0 {
            // SAFETY: as above.
            let m = unsafe {
                libc::splice(rd.as_raw_fd(), std::ptr::null_mut(), to.as_raw_fd(), std::ptr::null_mut(), left, libc::SPLICE_F_MOVE)
            };
            if m < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            if m == 0 {
                return;
            }
            left -= m as usize;
        }
    }
}

fn copy(from: &mut impl Read, to: &mut impl Write) {
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = to.flush();
}

/// `Read`/`Write` over a raw fd.
///
/// std has no owned-fd stream type that is not tied to a socket family it
/// knows, and vsock is not one of those.
struct Fd(OwnedFd);

impl Fd {
    fn try_clone(&self) -> std::io::Result<Fd> {
        self.0.try_clone().map(Fd)
    }
}

impl Read for Fd {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // SAFETY: reading into a buffer we own, with its true length.
        let n = unsafe { libc::read(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n as usize)
    }
}

impl Write for Fd {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // SAFETY: writing from a buffer we own, with its true length.
        let n = unsafe { libc::write(self.0.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
