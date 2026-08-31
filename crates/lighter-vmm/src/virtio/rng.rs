//! virtio-rng: host entropy for the guest.
//!
//! Small, but not optional. Without an entropy source a guest's
//! `getrandom(2)` blocks until the kernel has gathered enough on its own, and
//! in a VM with no real devices that can take a very long time. The visible
//! symptom is a boot that reaches userspace promptly and then sits for a minute
//! before the first process that needs randomness gets going — sshd, a TLS
//! handshake, or Docker generating an ID.

use std::io::Read;

use crate::memory::GuestMemory;
use crate::virtio::mmio::COMMON_FEATURES;
use crate::virtio::queue::Virtqueue;
use crate::virtio::{Serviced, VirtioDevice, device_type};

/// Cap on one fill, so a driver asking for an absurd buffer cannot make us
/// allocate it.
const MAX_FILL: usize = 1 << 20;

/// An entropy source for the guest.
pub struct Rng {
    source: Box<dyn Read + Send>,
}

impl Rng {
    /// Draws from the host's entropy pool.
    pub fn from_host() -> std::io::Result<Rng> {
        Ok(Rng {
            source: Box::new(std::fs::File::open("/dev/urandom")?),
        })
    }

    /// Draws from an arbitrary source, for tests.
    pub fn from_source(source: Box<dyn Read + Send>) -> Rng {
        Rng { source }
    }
}

impl VirtioDevice for Rng {
    fn device_type(&self) -> u32 {
        device_type::RNG
    }

    fn name(&self) -> &'static str {
        "virtio-rng"
    }

    fn features(&self) -> u64 {
        COMMON_FEATURES
    }

    fn queue_count(&self) -> usize {
        1
    }

    fn notify(&mut self, _queue: u16, queues: &mut [Virtqueue], mem: &GuestMemory) -> Serviced {
        let Some(queue) = queues.first_mut() else {
            return Serviced { used_any: false };
        };

        let mut used_any = false;
        while let Some(chain) = queue.pop(mem) {
            let head = chain.head();
            let mut filled = 0u32;

            for desc in chain {
                // The guest offers buffers for us to write; a readable
                // descriptor here is a malformed request.
                if !desc.is_write_only() {
                    continue;
                }
                let len = (desc.len as usize).min(MAX_FILL);
                let mut buf = vec![0u8; len];
                match self.source.read_exact(&mut buf) {
                    Ok(()) => {
                        if mem.write(desc.addr, &buf).is_ok() {
                            filled += len as u32;
                        }
                    }
                    Err(e) => {
                        // Returning the buffer short is legal and is far better
                        // than not returning it: the driver retries, whereas an
                        // unreturned buffer blocks the guest forever.
                        tracing::warn!(%e, "could not read host entropy");
                        break;
                    }
                }
            }

            queue.push_used(mem, head, filled);
            used_any = true;
        }

        Serviced { used_any }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_as_an_entropy_source() {
        let rng = Rng::from_source(Box::new(std::io::repeat(0x5a)));
        assert_eq!(rng.device_type(), device_type::RNG);
        assert_eq!(rng.queue_count(), 1);
    }

    #[test]
    fn the_host_pool_is_readable() {
        let mut rng = Rng::from_host().expect("/dev/urandom should open");
        let mut buf = [0u8; 32];
        rng.source.read_exact(&mut buf).unwrap();
        assert!(buf.iter().any(|&b| b != 0), "entropy source returned zeros");
    }
}
