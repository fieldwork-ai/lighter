//! The time, told to the guest when it asks.
//!
//! The guest has no real-time clock. Its monotonic clock is the host's
//! physical counter and cannot drift against the host, so the wallclock needs
//! setting exactly once, accurately, and again after the Mac has slept. It
//! used to be pushed: a whole number of seconds on the kernel command line,
//! captured before the kernel had started and applied after init had run,
//! and a second whole number over the control channel on wake, read once and
//! then retried for up to five seconds against an agent that was not yet
//! back. Three terms of error, half a second, the boot time, and the retries,
//! and every one of them second-shaped.
//!
//! Now the agent asks. It reads its monotonic clock, sends one byte, and gets
//! the host's wallclock in nanoseconds; half the round trip is the estimate
//! of how old that answer is when it arrives, and a vsock round trip is tens
//! of microseconds. The agent asks as soon as it runs, which is when the
//! answer is wanted, and again when the host nudges it after a wake. The
//! command-line seed stays for the seconds between the kernel and the agent,
//! when a certificate check is the only thing that cares.

use std::sync::Arc;

use crate::virtio::vsock::{Accepted, VsockShared};

/// The port the agent asks on.
pub const TIME_PORT: u32 = 2382;

/// One request, one answer: any byte in, twelve bytes out, seconds and then
/// nanoseconds since the epoch, both little-endian.
pub fn answer_now() -> [u8; 12] {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut out = [0u8; 12];
    out[..8].copy_from_slice(&now.as_secs().to_le_bytes());
    out[8..].copy_from_slice(&now.subsec_nanos().to_le_bytes());
    out
}

/// Answers the guest's questions for the machine's life. One thread: the
/// agent is the only client, and it asks a few times a boot.
pub fn start(shared: Arc<VsockShared>) -> std::io::Result<()> {
    let accepted = shared.listen(TIME_PORT);
    std::thread::Builder::new()
        .name("clock".into())
        .spawn(move || {
            for Accepted { key } in accepted {
                // Read the byte first and answer after: the answer's age is
                // measured by the guest from the moment it sent the byte.
                while shared.read_outbound_exact(key, 1).is_some() {
                    if !shared.send(key, &answer_now()) {
                        break;
                    }
                }
                shared.shutdown(key);
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_is_seconds_then_nanos_little_endian() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let out = answer_now();
        let secs = u64::from_le_bytes(out[..8].try_into().unwrap());
        let nanos = u32::from_le_bytes(out[8..].try_into().unwrap());
        assert!(secs >= before.as_secs());
        assert!(nanos < 1_000_000_000);
    }
}
