//! The machine's second card, on the Mac's network ("LAN mode").
//!
//! The first card (`net`) is a responder: every connection leaves the
//! guest as a stream, so the Mac's VPN and proxies apply and nothing is a
//! packet on any wire. This one is the opposite, a card bridged to one of
//! the Mac's own, carrying real frames to and from the user's network, so
//! that a host-network container can be on it: discover devices by mDNS,
//! SSDP and DHCP, be discovered, be reached at an address of its own.
//!
//! Frames come from a vmnet bridged interface relayed to a datagram socket,
//! one frame per datagram (`lighter_vmnet`). The relay runs in this process
//! when lighter holds `com.apple.vm.networking`, and in the root helper
//! (`lighter-bridge`) otherwise; the card sees the same socket either way.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;

use crate::virtio::net::{Inbox, Net, Outbox};

/// What keeps the bridge up: the helper's connection, or the relay itself.
enum Keep {
    Helper(#[allow(dead_code)] lighter_vmnet::helper::Held),
    InProcess(#[allow(dead_code)] lighter_vmnet::Bridge),
    #[cfg(test)]
    Nothing,
}

impl std::fmt::Debug for Lan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Lan({} {})",
            self.interface,
            lighter_vmnet::helper::mac_text(self.mac)
        )
    }
}

/// The card's host side.
pub struct Lan {
    frames: Arc<OwnedFd>,
    outbox: Arc<Outbox>,
    mac: [u8; 6],
    mtu: u16,
    interface: String,
    _keep: Keep,
}

impl Lan {
    /// Asks the root helper at `socket` to bridge `interface`.
    pub fn via_helper(socket: &Path, interface: &str, mac: [u8; 6]) -> io::Result<Lan> {
        let held = lighter_vmnet::helper::connect(socket, interface, mac)?;
        let frames = held.frames.try_clone()?;
        let mtu = held.mtu.clamp(576, 9000) as u16;
        Ok(Lan::with(frames, mtu, interface, mac, Keep::Helper(held)))
    }

    /// Bridges `interface` in this process, which needs root or
    /// `com.apple.vm.networking`.
    pub fn in_process(interface: &str, mac: [u8; 6]) -> Result<Lan, String> {
        let (bridge, frames) = lighter_vmnet::Bridge::start(interface, mac)?;
        let mtu = bridge.mtu.clamp(576, 9000) as u16;
        Ok(Lan::with(
            frames,
            mtu,
            interface,
            mac,
            Keep::InProcess(bridge),
        ))
    }

    fn with(frames: OwnedFd, mtu: u16, interface: &str, mac: [u8; 6], keep: Keep) -> Lan {
        Lan {
            frames: Arc::new(frames),
            outbox: Outbox::new(),
            mac,
            mtu,
            interface: interface.to_string(),
            _keep: keep,
        }
    }

    pub fn outbox(&self) -> Arc<Outbox> {
        self.outbox.clone()
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    pub fn mtu(&self) -> u16 {
        self.mtu
    }

    pub fn interface(&self) -> &str {
        &self.interface
    }

    /// The device for it: no offloads, since its frames reach a real wire.
    pub fn device(&self, inbox: Inbox) -> Net {
        Net::new_wired(self.outbox(), self.mac, inbox, self.mtu)
    }

    /// The card's two threads: the guest's frames to the network, and the
    /// network's into the guest's receive queue. A full socket either way
    /// drops a frame, as a full NIC ring does.
    pub fn spawn(
        &self,
        inbox: Inbox,
        wake_rx: impl Fn() + Send + 'static,
        wake_tx: impl Fn() + Send + 'static,
    ) -> io::Result<()> {
        let (frames, outbox) = (self.frames.clone(), self.outbox.clone());
        std::thread::Builder::new()
            .name("lan-tx".into())
            .spawn(move || {
                crate::qos::raise_interactive();
                while let Some((batch, parked)) = outbox.take() {
                    for frame in &batch {
                        // SAFETY: a frame buffer of its own length, on a live socket.
                        unsafe {
                            libc::send(
                                frames.as_raw_fd(),
                                frame.as_ptr().cast(),
                                frame.len(),
                                libc::MSG_DONTWAIT,
                            );
                        }
                    }
                    if parked {
                        wake_tx();
                    }
                }
                tracing::debug!("lan transmit stopped");
            })?;
        let frames = self.frames.clone();
        std::thread::Builder::new()
            .name("lan-rx".into())
            .spawn(move || {
                crate::qos::raise_interactive();
                let mut buf = vec![0u8; 65_550];
                loop {
                    // SAFETY: a buffer of its own length, on a live socket.
                    let n = unsafe {
                        libc::recv(frames.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0)
                    };
                    if n < 0 {
                        if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        break;
                    }
                    if n == 0 {
                        break;
                    }
                    let mut queued = Net::enqueue_received(&inbox, buf[..n as usize].to_vec());
                    // Everything already waiting goes in the same wake.
                    loop {
                        // SAFETY: as above, without waiting.
                        let more = unsafe {
                            libc::recv(
                                frames.as_raw_fd(),
                                buf.as_mut_ptr().cast(),
                                buf.len(),
                                libc::MSG_DONTWAIT,
                            )
                        };
                        if more <= 0 {
                            break;
                        }
                        queued |= Net::enqueue_received(&inbox, buf[..more as usize].to_vec());
                    }
                    if queued {
                        wake_rx();
                    }
                }
                tracing::info!("the LAN card's frames stopped");
            })?;
        Ok(())
    }
}

/// A MAC of the machine's own for the card: random, locally administered,
/// unicast; kept by the caller so that a router's lease survives restarts.
pub fn random_mac() -> [u8; 6] {
    let mut mac = [0u8; 6];
    // SAFETY: arc4random_buf fills the buffer it is given.
    unsafe { libc::arc4random_buf(mac.as_mut_ptr().cast(), mac.len()) };
    mac[0] = (mac[0] & 0xfc) | 0x02;
    mac
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_random_mac_is_local_and_unicast() {
        for _ in 0..100 {
            let mac = random_mac();
            assert_eq!(mac[0] & 0x01, 0, "unicast");
            assert_eq!(mac[0] & 0x02, 0x02, "locally administered");
        }
    }

    /// Frames cross both ways through a socket pair standing in for the
    /// relay: the guest's out, the network's in, waking the queue.
    #[test]
    fn frames_cross_both_ways() {
        let (near, far) = lighter_vmnet::socket_pair().unwrap();
        let lan = Lan::with(near, 1500, "en9", random_mac(), Keep::Nothing);
        let inbox = Net::new_inbox();
        let (woke_tx, woke_rx) = std::sync::mpsc::channel();
        lan.spawn(
            inbox.clone(),
            move || {
                let _ = woke_tx.send(());
            },
            || {},
        )
        .unwrap();
        // From the network.
        // SAFETY: a buffer of its length on a live socket.
        unsafe { libc::send(far.as_raw_fd(), [1u8; 60].as_ptr().cast(), 60, 0) };
        woke_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("a wake for the received frame");
        assert_eq!(inbox.lock().unwrap().pop_front().map(|f| f.len()), Some(60));
        // From the guest.
        lan.outbox.push(vec![2u8; 70]);
        let mut buf = [0u8; 128];
        let tv = libc::timeval {
            tv_sec: 2,
            tv_usec: 0,
        };
        unsafe {
            libc::setsockopt(
                far.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                std::ptr::addr_of!(tv).cast(),
                size_of::<libc::timeval>() as u32,
            )
        };
        let n = unsafe { libc::recv(far.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        assert_eq!(n, 70);
    }
}
