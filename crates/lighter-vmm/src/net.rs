//! The host side of guest networking.
//!
//! Guest traffic goes to a userspace network stack — `gvproxy`, from
//! containers/gvisor-tap-vsock — running as a sidecar process. It terminates
//! the guest's TCP flows onto ordinary host sockets, which is what lets a VM
//! reach the network with no privileged host device, no `pf` rules, and no
//! utun interface, and what makes it follow the Mac's own DNS and VPN routes.
//!
//! # Why a separate process
//!
//! It is written in Go, so it cannot be linked into a Rust VMM, and shelling
//! out to it is what podman, lima and vfkit all do. The seam is a documented
//! socket protocol rather than an API, which is also what makes it replaceable:
//! a native in-process stack would implement [`crate::virtio::net::NetBackend`]
//! and delete this file, with nothing else moving.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::virtio::net::{Inbox, Net, NetBackend, Outbox};

/// The address gvproxy hands the guest by DHCP, and its own gateway address.
/// Fixed by gvproxy's defaults; recorded here because the guest's routes and
/// any port-forward diagnostics have to agree with them.
pub const GATEWAY_IP: &str = "192.168.127.1";
pub const GUEST_IP: &str = "192.168.127.2";

/// The MAC gvproxy expects to see from the guest.
///
/// Its DHCP server keys the guest's lease on this address, so a different one
/// produces a link that is up and never gets an IP.
pub const GUEST_MAC: [u8; 6] = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee];

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error(
        "gvproxy was not found at {0}. Networking needs it; fetch it with \
         `scripts/fetch-gvproxy.sh`"
    )]
    NotFound(PathBuf),
    #[error("could not start gvproxy: {0}")]
    Spawn(#[source] io::Error),
    #[error("gvproxy did not create its socket at {path} within {timeout:?}")]
    NoSocket {
        path: PathBuf,
        timeout: std::time::Duration,
    },
    #[error("could not connect to gvproxy: {0}")]
    Connect(#[source] io::Error),
    #[error("gvproxy rejected {path}: {status} {body}")]
    Control {
        path: String,
        status: String,
        body: String,
    },
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Waits for gvproxy to create one of its listening sockets.
fn wait_for_socket(path: &Path, timeout: std::time::Duration) -> Result<(), NetError> {
    let deadline = std::time::Instant::now() + timeout;
    while !path.exists() {
        if std::time::Instant::now() > deadline {
            return Err(NetError::NoSocket {
                path: path.to_path_buf(),
                timeout,
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(())
}

/// A port on the host forwarded to the guest.
#[derive(Debug, Clone, Copy)]
pub struct PortForward {
    pub host_port: u16,
    pub guest_port: u16,
}

/// A running gvproxy, and the socket the VMM talks to it over.
pub struct Network {
    child: Child,
    socket_path: PathBuf,
    control_path: PathBuf,
    /// The socket. Written only by the transmit thread; cloned for the
    /// receive thread.
    stream: Arc<Mutex<UnixStream>>,
    /// Frames the device has taken off the guest's ring for the wire.
    outbox: Arc<Outbox>,
    /// The link's MTU, agreed with gvproxy at spawn.
    mtu: u16,
}

/// The MTU for the link to gvproxy.
///
/// gvproxy terminates every flow onto a host socket, so nothing past it sees
/// this number; it only decides how many frames a byte costs on the way
/// there. `LIGHTER_NET_MTU` overrides it for an A/B.
pub fn link_mtu() -> u16 {
    std::env::var("LIGHTER_NET_MTU")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|m| (68..=65_520).contains(m))
        .unwrap_or(crate::virtio::net::DEFAULT_MTU)
}

impl Network {
    /// Starts gvproxy and connects to it.
    pub fn start(gvproxy: &Path, run_dir: &Path, mtu: u16) -> Result<Network, NetError> {
        if !gvproxy.exists() {
            return Err(NetError::NotFound(gvproxy.to_path_buf()));
        }
        std::fs::create_dir_all(run_dir)?;

        let socket_path = run_dir.join("network.sock");
        let control_path = run_dir.join("gvproxy.sock");
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(&control_path);

        let mut command = Command::new(gvproxy);
        command
            // The control endpoint, used to add port forwards at runtime.
            .arg("--listen")
            .arg(format!("unix://{}", control_path.display()))
            // The data endpoint: a stream socket carrying length-prefixed
            // Ethernet frames, which is the protocol implemented below.
            .arg("--listen-qemu")
            .arg(format!("unix://{}", socket_path.display()))
            .arg("--mtu")
            .arg(mtu.to_string())
            // gvproxy's built-in SSH forward binds 127.0.0.1:2222 by default,
            // which we never use and which makes every second machine on the
            // Mac die at boot — the benchmark harness beside the daily
            // driver, most memorably. -1 turns the service off entirely.
            .arg("-ssh-port")
            .arg("-1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = command.spawn().map_err(NetError::Spawn)?;

        // gvproxy creates its listening sockets a moment after starting, so
        // connecting immediately races it. Both matter: the data socket carries
        // frames, and the control socket is the only way to add a port forward.
        let timeout = std::time::Duration::from_secs(5);
        wait_for_socket(&socket_path, timeout)?;
        wait_for_socket(&control_path, timeout)?;

        let stream = UnixStream::connect(&socket_path).map_err(NetError::Connect)?;
        crate::sockbuf::widen(&stream);
        tracing::info!(
            socket = %socket_path.display(),
            control = %control_path.display(),
            gateway = GATEWAY_IP,
            guest = GUEST_IP,
            mtu,
            "network started"
        );

        Ok(Network {
            child,
            socket_path,
            control_path,
            stream: Arc::new(Mutex::new(stream)),
            outbox: Outbox::new(),
            mtu,
        })
    }

    /// Forwards a host port to the same address inside the guest.
    ///
    /// Deliberately a runtime call rather than a start-up argument: Docker
    /// publishes ports when a container starts, which is long after the VM
    /// booted, so the set of forwards is never known at spawn time.
    pub fn expose(&self, forward: PortForward) -> Result<(), NetError> {
        let body = format!(
            r#"{{"local":"127.0.0.1:{}","remote":"{GUEST_IP}:{}"}}"#,
            forward.host_port, forward.guest_port
        );
        self.control("POST", "/services/forwarder/expose", &body)?;
        tracing::info!(
            host_port = forward.host_port,
            guest_port = forward.guest_port,
            "port forwarded"
        );
        Ok(())
    }

    /// Withdraws a forward added by [`Network::expose`].
    pub fn unexpose(&self, host_port: u16) -> Result<(), NetError> {
        let body = format!(r#"{{"local":"127.0.0.1:{host_port}"}}"#);
        self.control("POST", "/services/forwarder/unexpose", &body)?;
        Ok(())
    }

    /// The forwards gvproxy currently holds, as the JSON it reports them in.
    pub fn forwards(&self) -> Result<String, NetError> {
        self.control("GET", "/services/forwarder/all", "")
    }

    /// One request to gvproxy's control endpoint.
    ///
    /// Hand-rolled HTTP/1.0 over the unix socket rather than a client crate:
    /// the entire vocabulary is three fixed request shapes to a local socket we
    /// spawned ourselves, and `Connection: close` makes the response body
    /// exactly "everything until EOF" with no chunked encoding to parse.
    fn control(&self, method: &str, path: &str, body: &str) -> Result<String, NetError> {
        let mut stream = UnixStream::connect(&self.control_path).map_err(NetError::Connect)?;
        let request = format!(
            "{method} {path} HTTP/1.0\r\nHost: gvproxy\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes())?;
        stream.flush()?;

        let mut response = String::new();
        stream.read_to_string(&mut response)?;

        let (head, payload) = response
            .split_once("\r\n\r\n")
            .unwrap_or((response.as_str(), ""));
        let status = head.lines().next().unwrap_or("").to_string();
        if !status.contains(" 200") {
            return Err(NetError::Control {
                path: path.to_string(),
                status,
                body: payload.trim().to_string(),
            });
        }
        Ok(payload.to_string())
    }

    /// The link's MTU, for the device to advertise.
    pub const fn mtu(&self) -> u16 {
        self.mtu
    }

    /// Where the device puts frames for the wire.
    pub fn outbox(&self) -> Arc<Outbox> {
        self.outbox.clone()
    }

    /// Spawns the thread that moves transmitted frames from the outbox to
    /// the wire.
    ///
    /// This is the only thread that writes the socket, and the only one that
    /// may block on it. `wake` is called when the device had parked on a full
    /// outbox and there is room again, so the transport looks at the ring
    /// the device left chains on.
    pub fn spawn_transmitter(&self, wake: impl Fn() + Send + 'static) -> io::Result<()> {
        let outbox = self.outbox.clone();
        let mut backend = FramedStream {
            stream: self.stream.clone(),
        };
        std::thread::Builder::new()
            .name("net-tx".into())
            .spawn(move || {
                while let Some((frames, parked)) = outbox.take() {
                    if let Err(e) = backend.send_many(&frames) {
                        // The backend's problem, not the guest's: the frames
                        // are lost, exactly as they would be on a real
                        // network.
                        tracing::debug!(%e, dropped = frames.len(), "dropping transmitted frames");
                    }
                    if parked {
                        wake();
                    }
                }
                tracing::debug!("network transmitter stopped");
            })?;
        Ok(())
    }

    /// Spawns the thread that reads frames from the network into the guest.
    ///
    /// `wake` is called once per burst — after the last frame the socket had
    /// ready, not after each — so the transport moves a batch into the
    /// guest's receive queue under one lock and one interrupt. The reader
    /// itself touches no virtio state.
    pub fn spawn_receiver(&self, inbox: Inbox, wake: impl Fn() + Send + 'static) -> io::Result<()> {
        let mut reader = self
            .stream
            .lock()
            .expect("net stream poisoned")
            .try_clone()?;
        std::thread::Builder::new()
            .name("net-rx".into())
            .spawn(move || {
                // One read takes whatever the socket holds, up to a megabyte,
                // and the frames are cut out of that: two syscalls per frame
                // was a third of a frame's cost at 1500 bytes. A frame split
                // across two reads is carried over in `buf`.
                let mut buf = vec![0u8; 1 << 20];
                let mut filled = 0usize;
                let mut pending = 0usize;
                loop {
                    let n = match reader.read(&mut buf[filled..]) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    filled += n;
                    let mut at = 0usize;
                    while filled - at >= 4 {
                        let len = u32::from_be_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
                        if len == 0 || len > 65_550 {
                            // A length we cannot honour means the stream is
                            // out of sync, and there is no way to
                            // resynchronize a framed protocol other than to
                            // stop.
                            tracing::error!(len, "network stream framing lost");
                            return;
                        }
                        if filled - at - 4 < len {
                            break;
                        }
                        let frame = buf[at + 4..at + 4 + len].to_vec();
                        at += 4 + len;
                        if Net::enqueue_received(&inbox, frame) {
                            pending += 1;
                        } else {
                            tracing::trace!("receive backlog full; frame dropped");
                        }
                    }
                    // What is left is the start of a frame the next read
                    // completes.
                    buf.copy_within(at..filled, 0);
                    filled -= at;
                    // Keep reading while the socket has more: a burst is moved
                    // into the guest as one batch. A burst longer than the
                    // backlog is flushed part way so nothing is dropped for
                    // want of a look.
                    if pending > 0 && (bytes_ready(&reader) == 0 || pending >= 256) {
                        wake();
                        pending = 0;
                    }
                }
                tracing::debug!("network receiver stopped");
            })?;
        Ok(())
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn control_path(&self) -> &Path {
        &self.control_path
    }
}

impl Drop for Network {
    fn drop(&mut self) {
        self.outbox.close();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.control_path);
    }
}

/// The "qemu" wire protocol: a stream socket carrying each Ethernet frame
/// behind a 4-byte big-endian length.
struct FramedStream {
    stream: Arc<Mutex<UnixStream>>,
}

impl NetBackend for FramedStream {
    fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        let mut stream = self.stream.lock().expect("net stream poisoned");
        // Length and payload must reach the socket together: a partial write
        // between them desynchronizes the stream permanently, so they go in one
        // buffer rather than two writes.
        let mut buf = Vec::with_capacity(4 + frame.len());
        buf.extend_from_slice(&(frame.len() as u32).to_be_bytes());
        buf.extend_from_slice(frame);
        stream.write_all(&buf)
    }

    /// A batch goes to the socket as one `writev`: two iovecs per frame, the
    /// length and the payload, up to the platform's limit per call. One
    /// syscall per burst instead of one per frame is most of what a stream
    /// socket costs, and a partial write is completed before the next call
    /// so the framing stays intact.
    fn send_many(&mut self, frames: &[Vec<u8>]) -> io::Result<()> {
        // IOV_MAX on macOS; frames beyond it go in the next call.
        const IOVS_PER_CALL: usize = 1024;
        let stream = self.stream.lock().expect("net stream poisoned");
        let fd = stream.as_raw_fd();
        let headers: Vec<[u8; 4]> = frames
            .iter()
            .map(|f| (f.len() as u32).to_be_bytes())
            .collect();
        let mut iovs: Vec<libc::iovec> = Vec::with_capacity(frames.len() * 2);
        for (header, frame) in headers.iter().zip(frames) {
            iovs.push(libc::iovec {
                iov_base: header.as_ptr() as *mut libc::c_void,
                iov_len: 4,
            });
            iovs.push(libc::iovec {
                iov_base: frame.as_ptr() as *mut libc::c_void,
                iov_len: frame.len(),
            });
        }
        for chunk in iovs.chunks_mut(IOVS_PER_CALL) {
            write_all_vectored(fd, chunk)?;
        }
        Ok(())
    }
}

/// `writev` until every iovec is on the socket, advancing past whatever a
/// partial write took.
fn write_all_vectored(fd: libc::c_int, iovs: &mut [libc::iovec]) -> io::Result<()> {
    let mut first = 0usize;
    while first < iovs.len() {
        let rest = &iovs[first..];
        // SAFETY: every iovec points into a frame or header that outlives
        // this call, and the count is what the slice holds.
        let n = unsafe { libc::writev(fd, rest.as_ptr(), rest.len() as libc::c_int) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        let mut n = n as usize;
        while first < iovs.len() && n >= iovs[first].iov_len {
            n -= iovs[first].iov_len;
            first += 1;
        }
        if first < iovs.len() && n > 0 {
            // SAFETY: advancing within the same buffer by fewer bytes than
            // its length.
            iovs[first].iov_base = unsafe { iovs[first].iov_base.add(n) };
            iovs[first].iov_len -= n;
        }
    }
    Ok(())
}

/// Bytes the kernel holds for this socket that have not been read yet.
fn bytes_ready(stream: &UnixStream) -> usize {
    let mut n: libc::c_int = 0;
    // SAFETY: FIONREAD writes one int through the pointer given.
    let rc = unsafe { libc::ioctl(stream.as_raw_fd(), libc::FIONREAD, &mut n) };
    if rc < 0 { 0 } else { n.max(0) as usize }
}

/// The network as somewhere Docker's published ports can be sent.
///
/// The implementation is trivial; the point of the trait is direction. The
/// crate that understands Docker's API must not also have to understand
/// gvproxy, and this is the one place the two meet.
impl lighter_docker::PortMapper for Network {
    fn expose(&self, port: u16) -> Result<(), String> {
        Network::expose(
            self,
            PortForward {
                host_port: port,
                guest_port: port,
            },
        )
        .map_err(|e| e.to_string())
    }

    fn unexpose(&self, port: u16) -> Result<(), String> {
        Network::unexpose(self, port).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_length_prefixed_big_endian() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut backend = FramedStream {
            stream: Arc::new(Mutex::new(a)),
        };
        backend.send(&[0xde, 0xad, 0xbe, 0xef]).unwrap();

        let mut reader = b;
        let mut header = [0u8; 4];
        reader.read_exact(&mut header).unwrap();
        assert_eq!(u32::from_be_bytes(header), 4, "length must be big-endian");
        let mut payload = [0u8; 4];
        reader.read_exact(&mut payload).unwrap();
        assert_eq!(payload, [0xde, 0xad, 0xbe, 0xef]);
    }

    /// The DHCP lease is keyed on this address; a different one is a link that
    /// comes up and never gets an IP.
    #[test]
    fn guest_mac_matches_what_gvproxy_expects() {
        assert_eq!(GUEST_MAC, [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee]);
    }
}
