//! A unix socket on the host, carried to a port inside the guest.
//!
//! This is what makes `docker` on macOS talk to a `dockerd` in the VM: the CLI
//! connects to an ordinary socket in the user's home directory, and every
//! connection becomes a vsock connection to the agent listening in the guest.
//! Nothing about it is Docker-specific — it is a stream proxy — but Docker is
//! why it exists.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::virtio::vsock::{VsockShared, pump};

/// How long to wait for the guest to accept a connection.
///
/// Generous, because the first connection can arrive while the guest agent is
/// still starting. A client that gets a refusal retries a real request; one
/// that waits four seconds does not.
const ACCEPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

pub struct VsockProxy {
    path: PathBuf,
    stop: Arc<AtomicBool>,
}

impl VsockProxy {
    /// Starts accepting on `path`, forwarding each connection to `guest_port`.
    ///
    /// Delivery to the guest is [`VsockShared`]'s own responsibility, so there
    /// is nothing for a caller to poke.
    pub fn listen(
        path: &Path,
        guest_port: u32,
        shared: Arc<VsockShared>,
    ) -> io::Result<VsockProxy> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A socket file left by a previous run is not a live listener, and bind
        // fails with EADDRINUSE against it. Removing it is safe because a
        // running lighter holds the path open, not the inode.
        let _ = std::fs::remove_file(path);

        let listener = UnixListener::bind(path)?;
        let stop = Arc::new(AtomicBool::new(false));

        let accept_stop = stop.clone();
        let accept_shared = shared;

        std::thread::Builder::new()
            .name("vsock-accept".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if accept_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let shared = accept_shared.clone();
                    if let Err(e) = std::thread::Builder::new()
                        .name("vsock-conn".into())
                        .spawn(move || serve(stream, guest_port, shared))
                    {
                        tracing::warn!(%e, "could not spawn a connection thread");
                    }
                }
            })?;

        tracing::info!(path = %path.display(), guest_port, "proxying a socket into the guest");
        Ok(VsockProxy {
            path: path.to_path_buf(),
            stop,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for VsockProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Removing the path is what makes the accept loop's next blocking call
        // return; the thread is detached and ends with the process either way.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One accepted connection, from open to close.
fn serve(stream: UnixStream, guest_port: u32, shared: Arc<VsockShared>) {
    // The reader half goes to the pump; the writer half stays with the device,
    // which writes guest data into it from the vCPU thread servicing TX.
    let Ok(device_side) = stream.try_clone() else {
        return;
    };

    crate::sockbuf::widen(&stream);
    let host_port = shared.open(guest_port, device_side);
    if !shared.await_established(host_port, ACCEPT_TIMEOUT) {
        tracing::debug!(
            guest_port,
            "the guest did not accept; is the agent running?"
        );
        return;
    }

    pump(shared, host_port, stream);
}
