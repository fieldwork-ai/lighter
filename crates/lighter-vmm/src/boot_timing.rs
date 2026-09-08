//! Opt-in startup spans, shared by the CLI and its machine process.
//!
//! `LIGHTER_BOOT_TIMING=1` writes monotonic timestamps to stderr. It is a
//! diagnostic, not part of normal CLI output or the readiness protocol.

use std::time::Instant;

pub struct Phase {
    name: &'static str,
    start: Option<Instant>,
}

impl Phase {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            start: (std::env::var_os("LIGHTER_BOOT_TIMING").as_deref()
                == Some(std::ffi::OsStr::new("1")))
            .then(Instant::now),
        }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: clock_gettime fills this exclusively owned timespec.
            if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } == 0 {
                eprintln!(
                    "LIGHTER_BOOT_TIMING pid={} phase={} end_ns={} elapsed_us={}",
                    std::process::id(),
                    self.name,
                    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64,
                    start.elapsed().as_micros()
                );
            }
        }
    }
}
