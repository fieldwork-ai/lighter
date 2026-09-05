//! The host side of the guest console.
//!
//! Puts the controlling terminal in raw mode so the guest sees keystrokes as it
//! would on a serial line — no line buffering, no local echo, and control
//! characters delivered rather than interpreted by the host.
//!
//! Raw mode is a process-global side effect on a resource the user owns, so the
//! guard restores it on drop *and* on panic. A VMM that leaves a terminal in
//! raw mode because it crashed is a VMM whose users' shells stop echoing.

use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::devices::pl011::Pl011;

/// Restores the terminal's original settings when dropped.
pub struct RawMode {
    fd: RawFd,
    original: libc::termios,
}

impl RawMode {
    /// Puts stdin in raw mode, if it is a terminal.
    ///
    /// Returns `Ok(None)` when stdin is not a tty — a piped or redirected
    /// stdin needs no mode change, and forcing one would fail.
    pub fn enable() -> io::Result<Option<RawMode>> {
        let fd = io::stdin().as_raw_fd();
        // SAFETY: isatty takes a file descriptor and has no other effect.
        if unsafe { libc::isatty(fd) } != 1 {
            return Ok(None);
        }

        let mut original = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr fills the termios struct or returns non-zero.
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: tcgetattr succeeded, so the struct is initialized.
        let original = unsafe { original.assume_init() };

        let mut raw = original;
        // SAFETY: cfmakeraw only rewrites the struct we own.
        unsafe { libc::cfmakeraw(&mut raw) };
        // Block until at least one byte, with no inter-byte timer: the reader
        // is a dedicated thread, so blocking is what we want.
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;

        // SAFETY: `raw` is a fully initialized termios derived from the current
        // one.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Some(RawMode { fd, original }))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restoring settings captured from this same descriptor.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
        let _ = io::stdout().flush();
    }
}

/// Reads host input and feeds it to the guest's UART.
///
/// Runs until the machine stops. The thread is detached rather than joined:
/// it is blocked in `read(2)` on a terminal, and there is no portable way to
/// interrupt that without closing stdin out from under the user's shell.
pub fn spawn_input_thread(uart: Arc<Mutex<Pl011>>, running: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("console-input".into())
        .spawn(move || {
            let mut stdin = io::stdin();
            let mut buf = [0u8; 64];
            while running.load(Ordering::Relaxed) {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut uart = uart.lock().expect("uart mutex poisoned");
                        for byte in &buf[..n] {
                            if !uart.enqueue_input(*byte) {
                                // The guest is not draining. Dropping matches
                                // what a real UART does on overrun, and the
                                // alternative is unbounded growth.
                                break;
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .expect("failed to spawn console input thread");
}
