//! Printing to stdout when whoever reads it may have gone.
//!
//! `println!` panics when stdout is a pipe whose reader has closed it
//! (`lighter status | head -1`): Rust ignores SIGPIPE, so the write fails with
//! EPIPE rather than ending the process. Restoring SIGPIPE would not do: the
//! same commands write to the Docker socket and the machine's own, where a
//! peer hanging up, as the socket proxy does while the guest boots, must be an
//! error they report, not a silent death. So the reader leaving ends the
//! output and nothing else, and a command doing work (an upgrade) finishes it.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once a write has failed: nothing more is attempted.
static GONE: AtomicBool = AtomicBool::new(false);

pub fn write(args: std::fmt::Arguments<'_>) {
    if GONE.load(Ordering::Relaxed) {
        return;
    }
    if std::io::stdout().lock().write_fmt(args).is_err() {
        GONE.store(true, Ordering::Relaxed);
    }
}

/// `println!`, for output whose reader may leave.
macro_rules! outln {
    () => {
        $crate::output::write(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::output::write(format_args!("{}\n", format_args!($($arg)*)))
    };
}

/// `print!`, for output whose reader may leave.
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::output::write(format_args!($($arg)*))
    };
}
