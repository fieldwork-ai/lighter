//! The machine: what lighter's guest runs on, and the channels into it.
//!
//! The framework provides the machine itself (`vz.rs`); this module decides
//! what it is made of — the disks, the card, the console, the Rosetta share —
//! and starts the host halves of everything that talks to the guest: the
//! link (`link.rs`), the memory policy, the shares.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::balloon::Balloon;
use crate::disk::Disk;
use crate::link::{Hooks, Link};
use crate::net::{self, Network};
use crate::share::{Share, Shares};
use crate::vz::{self, Console, Event, Vm};

#[derive(Debug, thiserror::Error)]
pub enum MachineError {
    #[error("Virtualization.framework is not available on this Mac")]
    Unsupported,
    #[error("machine: {0}")]
    Framework(String),
    #[error("network: {0}")]
    Net(#[from] net::NetError),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone)]
pub struct MachineConfig {
    pub vcpus: u32,
    pub ram_bytes: u64,
    pub kernel: PathBuf,
    pub initramfs: Option<PathBuf>,
    pub cmdline: String,
    /// Attach the console to the terminal rather than only to stdout.
    pub interactive: bool,
    /// Disk images in device order; created at `disk_size_bytes` if absent.
    pub disks: Vec<PathBuf>,
    pub disk_size_bytes: u64,
    /// Whether the guest gets a network card.
    pub network: bool,
    pub run_dir: PathBuf,
    pub shares: Vec<Share>,
    /// Attach the Rosetta share when the Mac has Rosetta.
    pub rosetta: bool,
}

impl Default for MachineConfig {
    fn default() -> Self {
        MachineConfig {
            vcpus: 1,
            ram_bytes: 2 << 30,
            kernel: PathBuf::new(),
            initramfs: None,
            cmdline: String::new(),
            interactive: true,
            disks: Vec::new(),
            disk_size_bytes: 64 << 30,
            network: false,
            run_dir: std::env::temp_dir(),
            shares: Vec::new(),
            rosetta: true,
        }
    }
}

/// Why `wait` returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The guest powered off, asked or on its own.
    Shutdown,
    /// The framework stopped the machine.
    Error(String),
}

struct Proxy {
    path: PathBuf,
    stop: Arc<AtomicBool>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct Machine {
    vm: Arc<Vm>,
    link: Option<Arc<Link>>,
    balloon: Arc<Balloon>,
    disks: Vec<Arc<Disk>>,
    _memory_policy: Option<crate::memory_policy::MemoryPolicy>,
    _shares: Option<Arc<Shares>>,
    proxies: Vec<Proxy>,
    stopped: AtomicBool,
}

fn dup(fd: i32) -> io::Result<OwnedFd> {
    // SAFETY: dup of a descriptor the process holds; the result is ours.
    let raw = unsafe { libc::dup(fd) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn socketpair(kind: libc::c_int) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut pair = [0 as libc::c_int; 2];
    // SAFETY: socketpair into an array of two.
    if unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, pair.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors are fresh and ours.
    Ok(unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) })
}

impl Machine {
    pub fn start(config: &MachineConfig) -> Result<Machine, MachineError> {
        if !Vm::supported() {
            return Err(MachineError::Unsupported);
        }
        let mut disks = Vec::with_capacity(config.disks.len());
        for path in &config.disks {
            let disk = Arc::new(Disk::open_or_create(path, config.disk_size_bytes)?);
            tracing::info!(
                path = %path.display(),
                capacity_mib = disk.len() / (1 << 20),
                allocated_kib = disk.allocated_bytes().unwrap_or(0) / 1024,
                "attached disk"
            );
            disks.push(disk);
        }

        // The card: a datagram socketpair, the machine holding one end.
        let mtu = net::link_mtu();
        let card = if config.network {
            let (vm_end, host_end) = socketpair(libc::SOCK_DGRAM)?;
            for fd in [&vm_end, &host_end] {
                crate::sockbuf::widen_to(fd, 4 << 20);
            }
            Some((vm_end, host_end))
        } else {
            None
        };

        let rosetta = config.rosetta && vz::rosetta() == vz::Rosetta::Installed;
        if config.rosetta && !rosetta {
            tracing::info!(
                availability = ?vz::rosetta(),
                "Rosetta is not available; amd64 containers run under emulation"
            );
        }

        let vm_card = match &card {
            Some((vm_end, _)) => Some((dup(vm_end.as_raw_fd())?, mtu)),
            None => None,
        };
        let vm = Vm::start(vz::Config {
            cpus: config.vcpus as usize,
            memory_bytes: config.ram_bytes,
            kernel: config.kernel.clone(),
            initramfs: config.initramfs.clone(),
            cmdline: config.cmdline.clone(),
            console: Console {
                output: dup(1)?,
                input: if config.interactive { Some(dup(0)?) } else { None },
            },
            disks: config.disks.clone(),
            card: vm_card,
            mac: net::GUEST_MAC,
            rosetta,
        })
        .map_err(MachineError::Framework)?;
        let vm = Arc::new(vm);
        // For whoever measures the machine from outside (`lighter status`,
        // the benchmark harness): the helper's pid, beside our own.
        if let Some(pid) = vm.helper_pid() {
            let _ = std::fs::create_dir_all(&config.run_dir);
            let _ = std::fs::write(config.run_dir.join("helper.pid"), pid.to_string());
        }
        let balloon = Arc::new(Balloon::new(vm.clone(), config.ram_bytes));

        let memory_policy =
            match crate::memory_policy::MemoryPolicy::start(balloon.clone(), config.ram_bytes) {
                Ok(policy) => Some(policy),
                Err(why) => {
                    tracing::warn!(%why, "cannot watch host memory pressure; the guest keeps whatever it takes");
                    None
                }
            };

        let shares = if config.shares.is_empty() {
            None
        } else {
            Some(Arc::new(Shares::new(&config.shares)?))
        };

        let link = match card {
            Some((_vm_end, host_end)) => {
                let network = Network::start(mtu)?;
                let offers = memory_policy
                    .as_ref()
                    .map(|p| p.offers())
                    .unwrap_or_else(|| Arc::new(|_, _| {}));
                let link = Link::start(
                    host_end,
                    mtu,
                    network,
                    Hooks {
                        memory: offers,
                        shares: shares.clone().map(|s| s as Arc<dyn crate::link::ShareTransport>),
                    },
                )?;
                if let Some(shares) = &shares {
                    shares.attach(link.clone());
                }
                Some(link)
            }
            None => None,
        };

        tracing::info!(
            vcpus = config.vcpus,
            ram_mib = config.ram_bytes / (1 << 20),
            rosetta,
            "machine started"
        );

        Ok(Machine {
            vm,
            link,
            balloon,
            disks,
            _memory_policy: memory_policy,
            _shares: shares,
            proxies: Vec::new(),
            stopped: AtomicBool::new(false),
        })
    }

    /// The link into the guest, when the machine has a card.
    pub fn link(&self) -> Option<Arc<Link>> {
        self.link.clone()
    }

    /// A unix socket on the host, carried to a port inside the guest: what
    /// makes `docker` on the Mac talk to `dockerd` in the machine.
    pub fn proxy_socket(&mut self, path: &Path, guest_port: u16) -> io::Result<()> {
        let Some(link) = self.link.clone() else {
            return Err(io::Error::other("no network, so nothing to proxy over"));
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(path);
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        let stop = Arc::new(AtomicBool::new(false));
        let accept_stop = stop.clone();
        std::thread::Builder::new()
            .name(format!("proxy-{guest_port}"))
            .spawn(move || {
                for stream in listener.incoming() {
                    if accept_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    link.proxy(guest_port, OwnedFd::from(stream));
                }
            })?;
        tracing::info!(path = %path.display(), guest_port, "proxying a socket into the guest");
        self.proxies.push(Proxy {
            path: path.to_path_buf(),
            stop,
        });
        Ok(())
    }

    /// One line to the agent's control channel and its one-line answer.
    pub fn control(&self, line: &str, timeout: Duration) -> io::Result<String> {
        use std::io::{BufRead, BufReader, Write};
        let Some(link) = self.link.clone() else {
            return Err(io::Error::other("no network, so no control channel"));
        };
        let (ours, theirs) = socketpair(libc::SOCK_STREAM)?;
        link.proxy(crate::link::CONTROL_PORT, theirs);
        let mut stream = std::os::unix::net::UnixStream::from(ours);
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.write_all(format!("{line}\n").as_bytes())?;
        let mut reply = String::new();
        BufReader::new(&stream).read_line(&mut reply)?;
        Ok(reply.trim_end().to_string())
    }

    /// Blocks until the machine stops.
    pub fn wait(&self) -> Result<StopReason, MachineError> {
        loop {
            match self.vm.next_event(Duration::from_secs(1)) {
                Some(Event::GuestStopped) => return Ok(StopReason::Shutdown),
                Some(Event::StoppedWithError(e)) => return Ok(StopReason::Error(e)),
                None => {
                    if self.stopped.load(Ordering::Relaxed) {
                        return Ok(StopReason::Shutdown);
                    }
                }
            }
        }
    }

    pub fn balloon(&self) -> &Arc<Balloon> {
        &self.balloon
    }

    pub fn disks(&self) -> &[Arc<Disk>] {
        &self.disks
    }

    /// Asks the guest to power off, and stops the machine if it does not
    /// within a few seconds.
    pub fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::Relaxed) {
            return;
        }
        let asked = self
            .control("poweroff", Duration::from_secs(2))
            .map(|r| r == "ok")
            .unwrap_or(false);
        let deadline = Instant::now() + Duration::from_secs(if asked { 8 } else { 1 });
        while Instant::now() < deadline {
            match self.vm.next_event(Duration::from_millis(200)) {
                Some(Event::GuestStopped) | Some(Event::StoppedWithError(_)) => return,
                None => {}
            }
        }
        tracing::warn!(asked, "the guest did not power off; stopping the machine");
        if let Err(e) = self.vm.stop() {
            tracing::warn!(%e, "stop failed");
        }
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        if !self.stopped.load(Ordering::Relaxed) {
            let _ = self.vm.stop();
        }
    }
}
