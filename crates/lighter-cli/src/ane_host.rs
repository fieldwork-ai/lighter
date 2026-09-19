//! The Neural Engine service as a process of its own.
//!
//! ONNX Runtime with CoreML runs a model Apple's frameworks compile and
//! execute, and those crash on some graphs: Piper's voice model took the
//! whole machine down through a segfault in an Espresso convolution kernel,
//! with the home server on it. So the service runs in a child of the CLI,
//! the same binary with a hidden subcommand, on a loopback port the parent
//! chose and the guest was told; when it dies it is started again on that
//! port, the container's request fails once, and ONNX Runtime in the
//! container falls back to its CPU provider for that run. The runner also
//! remembers which candidate it was running when it died, so the next load
//! of that model skips it. The PyTorch device is arranged the same way.
//!
//! The child dies with the parent however the parent dies: it reads a pipe
//! the parent holds the writing end of, and `lighter stop` ends the
//! machine with a signal that runs no destructor, so the pipe's EOF is the
//! only word it gets. Without it every machine left its service behind.

use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The child and the writing end of its stdin, held apart from it: `wait`
/// closes a `Child`'s own stdin handle before waiting, which would be the
/// EOF the child exits on.
struct Running {
    child: Child,
    _stdin: ChildStdin,
}

pub struct Supervisor {
    port: u16,
    stopping: Arc<AtomicBool>,
    child: Arc<std::sync::Mutex<Option<Running>>>,
}

impl Supervisor {
    /// Picks a free loopback port, starts the service on it, and keeps it
    /// started until dropped.
    pub fn start(cache: &Path) -> std::io::Result<Supervisor> {
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
            .local_addr()?
            .port();
        let exe = std::env::current_exe()?;
        let child = spawn(&exe, port, cache)?;
        let stopping = Arc::new(AtomicBool::new(false));
        let child = Arc::new(std::sync::Mutex::new(Some(child)));
        {
            let stopping = stopping.clone();
            let child = child.clone();
            let cache = cache.to_path_buf();
            std::thread::Builder::new()
                .name("ane-supervise".into())
                .spawn(move || supervise(exe, port, cache, stopping, child))?;
        }
        Ok(Supervisor {
            port,
            stopping,
            child,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(mut running) = self.child.lock().ok().and_then(|mut c| c.take()) {
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }
}

/// Starts the service and waits for its "ready" line, so a port the guest
/// is told is one that answers.
fn spawn(exe: &Path, port: u16, cache: &Path) -> std::io::Result<Running> {
    let mut child = Command::new(exe)
        .arg("ane-host")
        .arg("--port")
        .arg(port.to_string())
        .arg("--cache")
        .arg(cache)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped");
    let stdin = child.stdin.take().expect("piped");
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line)?;
    if line.trim() != "ready" {
        let _ = child.kill();
        return Err(std::io::Error::other(format!(
            "the neural engine service did not start: {}",
            line.trim()
        )));
    }
    Ok(Running {
        child,
        _stdin: stdin,
    })
}

fn supervise(
    exe: PathBuf,
    port: u16,
    cache: PathBuf,
    stopping: Arc<AtomicBool>,
    slot: Arc<std::sync::Mutex<Option<Running>>>,
) {
    loop {
        // Taken out of the slot to wait on, with the stdin handle kept open
        // beside it for as long as it runs.
        let Some(mut running) = slot.lock().ok().and_then(|mut c| c.take()) else {
            return;
        };
        let status = running.child.wait();
        drop(running);
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        tracing::warn!(
            ?status,
            port,
            "the neural engine service died; starting it again"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
        match spawn(&exe, port, &cache) {
            Ok(running) => {
                if let Ok(mut c) = slot.lock() {
                    *c = Some(running);
                }
            }
            Err(e) => {
                tracing::error!(%e, "the neural engine service could not be started again");
                return;
            }
        }
    }
}

/// The child's side: serve on the port until killed. Prints "ready" once
/// the port is bound, or the error.
pub fn serve(port: u16, cache: &Path) -> anyhow::Result<()> {
    // The parent's end of stdin closes when the parent is gone, by any
    // route; then so is the reason to run.
    std::thread::Builder::new()
        .name("ane-parent".into())
        .spawn(|| {
            use std::io::Read;
            let mut sink = [0u8; 64];
            while matches!(std::io::stdin().lock().read(&mut sink), Ok(n) if n > 0) {}
            std::process::exit(0);
        })?;
    // The parent's stderr is the machine log; this process's lines land
    // there beside it, placement numbers included.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LIGHTER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    match lighter_vmm::ane::Server::start_at(port, Some(cache)) {
        Ok(_server) => {
            println!("ready");
            loop {
                std::thread::park();
            }
        }
        Err(e) => {
            println!("{e}");
            Err(e.into())
        }
    }
}
