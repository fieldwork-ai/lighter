//! The lighter guest agent.
//!
//! Runs inside the VM and answers vsock connections from the host. Its whole
//! job is to be the guest end of a stream: the host's `~/.lighter/docker.sock`
//! becomes a vsock connection here, and this bridges it to the real
//! `/run/docker.sock` that dockerd is listening on.
//!
//! It is a separate binary rather than part of init because it must be
//! restartable without restarting PID 1, and separate from the VMM because it
//! is a Linux program.
//!
//! ```text
//!   docker CLI ──unix──▶ lighter ──vsock──▶ agent ──unix──▶ dockerd
//! ```

mod vsock;

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use vsock::VsockListener;

fn main() -> std::process::ExitCode {
    let mut port: u32 = 2375;
    let mut target: Option<String> = None;
    let mut echo = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or(2375),
            "--to" => target = args.next(),
            // Answers connections itself instead of bridging. The vsock gate
            // uses it to prove the transport with nothing else installed.
            "--echo" => echo = true,
            other => {
                eprintln!("lighter-agent: unknown argument {other}");
                return std::process::ExitCode::from(2);
            }
        }
    }

    if target.is_none() && !echo {
        eprintln!("lighter-agent: one of --to <path> or --echo is required");
        return std::process::ExitCode::from(2);
    }

    let listener = match VsockListener::bind(port) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("lighter-agent: cannot bind vsock port {port}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // The line the gate waits for. Printed only once the socket is bound, so
    // seeing it means a connection now would be accepted rather than refused.
    println!("AGENT listening port={port}");

    loop {
        let stream = match listener.accept() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("lighter-agent: accept failed: {e}");
                continue;
            }
        };
        let target = target.clone();
        std::thread::spawn(move || match target {
            Some(path) => bridge(stream, &path),
            None => echo_back(stream),
        });
    }
}

/// Copies between the vsock connection and a unix socket until either ends.
fn bridge(guest_side: OwnedFd, path: &str) {
    let upstream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(e) => {
            // The errno is the whole diagnosis here: "no such file" means the
            // daemon has not created its socket yet, "connection refused"
            // means it died after creating one, and "permission denied" means
            // something quite different again. Reporting only the path sends
            // you looking in the wrong place.
            eprintln!("lighter-agent: cannot reach {path}: {e}");
            return;
        }
    };
    let Ok(mut upstream_read) = upstream.try_clone() else {
        return;
    };
    let mut upstream_write = upstream;

    // Two threads rather than poll(): the whole point of this process is to be
    // simple enough to trust, and a stream copy in each direction is the
    // simplest thing that cannot deadlock on a half-full pipe.
    //
    // The two directions are named rather than inferred. They were once both
    // written as "guest to upstream" — which connects, forwards the request,
    // and then hangs forever waiting for a reply nobody is carrying back.
    let mut guest_read = Fd(guest_side);
    let mut guest_write = match guest_read.try_clone() {
        Ok(fd) => fd,
        Err(_) => return,
    };

    // guest -> dockerd
    let request = std::thread::spawn(move || {
        copy(&mut guest_read, &mut upstream_write);
        // Let dockerd see the end of the request rather than waiting on a
        // connection that will send no more.
        let _ = upstream_write.shutdown(std::net::Shutdown::Write);
    });

    // dockerd -> guest
    copy(&mut upstream_read, &mut guest_write);

    let _ = request.join();
}

/// Sends back whatever arrives, so the gate can prove the round trip.
fn echo_back(stream: OwnedFd) {
    let mut fd = Fd(stream);
    let mut buf = [0u8; 4096];
    loop {
        match fd.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if fd.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
        }
    }
}

fn copy(from: &mut impl Read, to: &mut impl Write) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = to.flush();
}

/// `Read`/`Write` over a raw fd.
///
/// std has no owned-fd stream type that is not tied to a socket family it
/// knows, and vsock is not one of those.
struct Fd(OwnedFd);

impl Fd {
    fn try_clone(&self) -> std::io::Result<Fd> {
        self.0.try_clone().map(Fd)
    }
}

impl Read for Fd {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // SAFETY: reading into a buffer we own, with its true length.
        let n = unsafe { libc::read(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n as usize)
    }
}

impl Write for Fd {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // SAFETY: writing from a buffer we own, with its true length.
        let n = unsafe { libc::write(self.0.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
