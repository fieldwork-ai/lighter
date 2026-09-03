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

mod vsock;

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use vsock::VsockListener;

fn main() -> std::process::ExitCode {
    let mut port: u32 = 2375;
    let mut target: Option<String> = None;
    let mut echo = false;
    let mut control = false;

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
            other => {
                eprintln!("lighter-agent: unknown argument {other}");
                return std::process::ExitCode::from(2);
            }
        }
    }

    if target.is_none() && !echo && !control {
        eprintln!("lighter-agent: one of --to <path>, --echo or --control is required");
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
/// been idle for half a minute the cache is trimmed to a quarter of RAM,
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
    let resting = total / 4;
    let cgroup = "/sys/fs/cgroup/docker";
    let mut idle_for = 0u32;
    let mut last = container_cpu_usec(cgroup);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let now = container_cpu_usec(cgroup);
        let used = now.saturating_sub(last);
        last = now;
        idle_for = if used < 50_000 { idle_for + 1 } else { 0 };
        if idle_for < 10 {
            continue;
        }
        let current = std::fs::read_to_string(format!("{cgroup}/memory.current"))
            .ok()
            .and_then(|c| c.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if current > resting {
            let _ = std::fs::write(format!("{cgroup}/memory.reclaim"), (current - resting).to_string());
        }
        // Trimmed: not again until the containers have worked and rested.
        idle_for = 0;
    }
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

fn copy(from: &mut impl Read, to: &mut impl Write) {
    let mut buf = vec![0u8; 64 * 1024];
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
