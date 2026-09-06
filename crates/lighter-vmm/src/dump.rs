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

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use crate::devices::pl011::Pl011;
use crate::virtio::mmio::VirtioMmio;
use crate::virtio::vsock::VsockShared;

static PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(signal: libc::c_int) {
    let fd = PIPE_WRITE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte: &[u8; 1] = if signal == libc::SIGUSR2 { b"i" } else { b"d" };
        // SAFETY: a plain write of one byte to a pipe we own; async-signal-safe.
        let _ = unsafe { libc::write(fd, byte.as_ptr().cast(), 1) };
    }
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
