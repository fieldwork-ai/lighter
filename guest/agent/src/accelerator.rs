//! The idle-poll window while a container talks to an accelerator.
//!
//! A stream to one of the host's accelerator servers (the Neural Engine,
//! ggml on Metal, PyTorch on MPS) is a request loop: llama.cpp sends eight
//! messages a token and waits for the logits, and its client hands every
//! message between two threads. Each wait is a vCPU going idle, and a vCPU
//! that has gone through WFI is woken by the host, late on a busy Mac. The
//! guest kernel polls before WFI (patch 0011) for a window that adapts
//! between a floor and a cap, 50 µs to 200 µs at rest, sized for a machine
//! whose idle should cost the Mac nothing. While a model is answering the
//! opposite is wanted: the vCPUs stay awake between messages, as a native
//! client's threads do. On the M1, Qwen2.5-0.5B over `lighter.sh/metal`:
//! 64 tokens a second at the resting window, 76 with a 2 ms cap, 95 to 99
//! with 5 ms, and no more at 20 or 50, against 107 for a native client of
//! the same server. A token's wait for the logits is nine milliseconds, so
//! 5 ms is the cap that keeps the vCPU awake between messages and lets it
//! sleep through the compute.
//!
//! So the window is widened for exactly as long as a stream to an
//! accelerator port is open, and restored when the last one closes. The
//! ports come from the kernel command line, where init also reads them for
//! the CDI specs. The sysfs knobs are the patch's module parameters.

use std::sync::{Mutex, OnceLock};

const POLL_NS: &str = "/sys/module/idle/parameters/poll_ns";
const POLL_GROW_START_NS: &str = "/sys/module/idle/parameters/poll_grow_start_ns";

/// The cap and the floor while a stream is open, nanoseconds.
const WIDE_NS: &str = "5000000";
const WIDE_START_NS: &str = "2000000";

/// The keys init publishes as devices; each names a host loopback port.
const KEYS: [&str; 3] = ["lighter.ane", "lighter.metal", "lighter.mps"];

fn ports() -> &'static [u16] {
    static PORTS: OnceLock<Vec<u16>> = OnceLock::new();
    PORTS.get_or_init(|| {
        let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
        cmdline
            .split_whitespace()
            .filter_map(|w| {
                let (key, value) = w.split_once('=')?;
                KEYS.contains(&key).then(|| value.parse().ok()).flatten()
            })
            .collect()
    })
}

/// Streams open to accelerator ports, and the resting values to put back.
static OPEN: Mutex<(u32, Option<(String, String)>)> = Mutex::new((0, None));

/// Held for the life of a stream to an accelerator port; `None` for any
/// other stream.
pub struct Wide(());

impl Wide {
    pub fn open(port: u16) -> Option<Wide> {
        if !ports().contains(&port) {
            return None;
        }
        let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
        if open.0 == 0 {
            let resting = (
                std::fs::read_to_string(POLL_NS).ok()?,
                std::fs::read_to_string(POLL_GROW_START_NS).ok()?,
            );
            // The floor first, so the cap is never below it.
            if std::fs::write(POLL_GROW_START_NS, WIDE_START_NS).is_err()
                || std::fs::write(POLL_NS, WIDE_NS).is_err()
            {
                return None;
            }
            open.1 = Some(resting);
        }
        open.0 += 1;
        Some(Wide(()))
    }
}

impl Drop for Wide {
    fn drop(&mut self) {
        let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
        open.0 -= 1;
        if open.0 == 0
            && let Some((poll_ns, start_ns)) = open.1.take()
        {
            let _ = std::fs::write(POLL_NS, poll_ns.trim());
            let _ = std::fs::write(POLL_GROW_START_NS, start_ns.trim());
        }
    }
}
