//! `kill -INFO <vmm pid>` prints every virtqueue's state and the vsock
//! counters to stderr; `kill -USR2` raises every device's interrupt line.
//! USR1 also dumps in the bench runner, but the CLI reserves it for stopping
//! its Docker event watcher. INFO remains available in either process.
//! With `LIGHTER_DEBUG_SYSRQ=w` (blocked tasks) or `t` (all tasks), INFO also
//! sends a serial SysRq after the host dump. This can wake a stalled guest;
//! leave it unset when observing the queue state without an injected IRQ.
//!
//! It is the one probe that works on a machine whose control channel is the
//! thing that has stopped answering: a guest asleep on an interrupt it never
//! received looks, from outside, exactly like an idle one, and the ring
//! indices are what tell the two apart. The handler only writes a byte to a
//! pipe; a thread parked on the other end does the reading and printing.
//!
//! The guest asks for the same dump itself when its streams to the host have
//! all timed out (`host_lost` in the agent), through the one path left to it,
//! the console, and the report adds `sample`'s account of every thread here:
//! a stall nobody is watching when it happens names its stuck thread in the
//! log it leaves (#84).

use std::io::Write;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::devices::pl011::Pl011;
use crate::virtio::mmio::VirtioMmio;
use crate::virtio::vsock::VsockShared;

static PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

/// What the agent prints, once, when the host has stopped answering it.
const HOST_LOST: &[u8] = b"lighter-agent: the host has stopped answering";

fn request(byte: u8) {
    let fd = PIPE_WRITE.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: a plain write of one byte to a pipe we own; async-signal-safe.
        let _ = unsafe { libc::write(fd, [byte].as_ptr().cast(), 1) };
    }
}

/// The guest console's sink, watching for the agent's word that the host
/// has stopped answering.
pub struct HostLostWatch<W> {
    inner: W,
    line: Vec<u8>,
    reported: bool,
}

impl<W> HostLostWatch<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            line: Vec::with_capacity(HOST_LOST.len()),
            reported: false,
        }
    }
}

impl<W: Write> Write for HostLostWatch<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        for &byte in &buf[..n] {
            if byte == b'\n' {
                if self.line == HOST_LOST && !self.reported {
                    self.reported = true;
                    request(b'h');
                }
                self.line.clear();
            } else if self.line.len() < HOST_LOST.len() {
                self.line.push(byte);
            }
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

extern "C" fn on_signal(signal: libc::c_int) {
    request(if signal == libc::SIGUSR2 { b'i' } else { b'd' });
}

pub fn install(
    virtio: Vec<Arc<Mutex<VirtioMmio>>>,
    vsock: Arc<VsockShared>,
    uart: Arc<Mutex<Pl011>>,
) {
    // Explicit opt-in: ordinary INFO dumps do not inject a guest interrupt.
    let sysrq = match std::env::var("LIGHTER_DEBUG_SYSRQ").as_deref() {
        Ok("w") => Some(b'w'),
        Ok("t") => Some(b't'),
        _ => None,
    };
    let mut fds = [0i32; 2];
    // SAFETY: pipe(2) writes two descriptors into the array we hand it.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return;
    }
    PIPE_WRITE.store(fds[1], Ordering::Relaxed);
    // SAFETY: installing a handler that does nothing but write to a pipe.
    unsafe {
        libc::signal(
            libc::SIGUSR1,
            on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGUSR2,
            on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINFO,
            on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
    let read = fds[0];
    let _ = std::thread::Builder::new()
        .name("debug-dump".into())
        .spawn(move || {
            loop {
                let mut byte = [0u8; 1];
                // SAFETY: reading one byte into a buffer of one.
                let n = unsafe { libc::read(read, byte.as_mut_ptr().cast(), 1) };
                if n < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                if n <= 0 {
                    break;
                }
                if byte[0] == b'i' {
                    for (slot, transport) in virtio.iter().enumerate() {
                        match transport.try_lock() {
                            Ok(transport) => transport.debug_interrupt(),
                            Err(_) => eprintln!("DUMP slot{slot} locked, no interrupt"),
                        }
                    }
                    eprintln!("DUMP interrupted every device");
                    continue;
                }
                if byte[0] == b'h' {
                    eprintln!(
                        "DUMP the guest reports the host has stopped answering; sampling this process"
                    );
                    sample();
                }
                eprintln!("DUMP begin");
                for (slot, transport) in virtio.iter().enumerate() {
                    match transport.try_lock() {
                        Ok(transport) => {
                            for line in transport.debug_dump() {
                                eprintln!("DUMP slot{slot} {line}");
                            }
                        }
                        Err(_) => eprintln!("DUMP slot{slot} locked"),
                    }
                }
                for line in vsock.trace_lines() {
                    eprintln!("DUMP vsock {line}");
                }
                eprintln!("DUMP end");
                if let Some(key) = sysrq {
                    match uart.try_lock() {
                        Ok(mut uart) => {
                            eprintln!("DUMP sysrq queued={}", uart.enqueue_sysrq(key));
                        }
                        Err(_) => eprintln!("DUMP uart locked, no sysrq"),
                    }
                }
            }
        });
}

/// Every thread's stack in this process, three seconds of them, into the
/// log. `sample` reads them from outside, so a thread stuck on a lock is
/// seen where it is stuck; a hardened-runtime process allows it to its own
/// user.
fn sample() {
    let path = std::env::temp_dir().join(format!("lighter-sample-{}.txt", std::process::id()));
    let child = std::process::Command::new("/usr/bin/sample")
        .arg(std::process::id().to_string())
        .arg("3")
        .arg("-file")
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            eprintln!("DUMP sample did not start: {e}");
            return;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    match std::fs::read_to_string(&path) {
        Ok(report) => {
            let mut err = std::io::stderr().lock();
            for line in report.lines() {
                let _ = writeln!(err, "DUMP sample {line}");
            }
        }
        Err(e) => eprintln!("DUMP sample produced nothing ({status:?}): {e}"),
    }
    let _ = std::fs::remove_file(&path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agents_host_lost_line_is_noticed_and_passed_through() {
        let mut watch = HostLostWatch::new(Vec::new());
        let quiet = b"lighter-agent: stream to host refused: Operation timed out (os error 110)\n\
            [10983.425589] sysrq: lighter-agent: the host has stopped answering\n";
        for byte in quiet {
            watch.write_all(&[*byte]).unwrap();
        }
        assert!(!watch.reported);
        let lost =
            b"lighter-agent: the host has stopped answering; dumping the guest's state to the console\n";
        for byte in lost {
            watch.write_all(&[*byte]).unwrap();
        }
        assert!(watch.reported);
        assert_eq!(watch.inner, [&quiet[..], &lost[..]].concat());
    }
}
