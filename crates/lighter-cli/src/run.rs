//! Being the machine.
//!
//! `lighter run` is the process the VM lives in. It is hidden from the help
//! because nobody should type it — `lighter start` spawns it, and launchd
//! spawns it — but it is an ordinary foreground program, which is what makes
//! it debuggable: run it by hand and the console output is the guest's.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lighter_vmm::virtio::fs::Share;
use lighter_vmm::wake::{Observer, Power};
use lighter_vmm::{Machine, MachineConfig};

use crate::config::Config;
use crate::paths;

/// Builds the machine described by the configuration and runs it until it
/// stops.
/// The machine's own copy of the root filesystem.
///
/// The master in the guest directory is an artifact, and mounting an
/// artifact read-write is how it stopped being one: the running machine
/// dirtied it continuously, a gate or benchmark booting the same file
/// beside it would have corrupted both, and copying it anywhere produced a
/// torn snapshot. The machine clones it into its own home instead — an APFS
/// clonefile, so the copy is instant and costs nothing until blocks diverge
/// — refreshed whenever the master is newer, which is what makes `make
/// guest` reach the next start.
fn private_rootfs() -> anyhow::Result<std::path::PathBuf> {
    let master = paths::rootfs()?;
    let private = paths::home()?.join("rootfs.ext4");
    // Which master the copy was made from, kept beside it. The copy's own
    // modification time says nothing: the guest writes to its root disk, so
    // a running machine keeps its copy newer than any master, and a check
    // on the two times never refreshed it — a daily driver ran a rootfs
    // three builds old while every benchmark VM booted the current one.
    let stamp = paths::home()?.join("rootfs.ext4.from");
    let current = if let Some(root) = crate::installation::payload(&std::env::current_exe()?)
        && let Ok(manifest) = crate::release::read(&root)
        && std::env::var_os("LIGHTER_GUEST_DIR").is_none_or(|p| {
            std::path::PathBuf::from(p).canonicalize().ok()
                == root.join("share/lighter").canonicalize().ok()
        }) {
        format!("sha256:{}", manifest.files["share/lighter/rootfs.ext4"])
    } else {
        let meta = std::fs::metadata(&master)?;
        format!("{:?} {}", meta.modified()?, meta.len())
    };
    let stale =
        !private.exists() || std::fs::read_to_string(&stamp).map_or(true, |from| from != current);
    if stale {
        let staging = paths::home()?.join(".rootfs.next");
        let _ = std::fs::remove_file(&staging);
        clonefile(&master, &staging)?;
        std::fs::rename(&staging, &private)?;
        std::fs::write(&stamp, &current)?;
    }
    Ok(private)
}

/// An APFS clonefile, falling back to a plain copy on filesystems without it.
fn clonefile(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<()> {
    let src = std::ffi::CString::new(from.as_os_str().as_encoded_bytes())?;
    let dst = std::ffi::CString::new(to.as_os_str().as_encoded_bytes())?;
    // SAFETY: two valid NUL-terminated paths.
    if unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    Ok(())
}

pub fn machine() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LIGHTER_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let config = Config::load()?;
    let home = paths::home()?;
    std::fs::create_dir_all(&home)?;

    let selection_lease = crate::upgrade::start_lease()?;
    let Some(mut instance) = crate::instance::Instance::acquire(&home)? else {
        eprintln!("lighter is already running; this copy has nothing to do");
        return Ok(());
    };

    let shares = config
        .shares
        .iter()
        .enumerate()
        .map(|(index, path)| Share {
            // A tag is limited to 36 bytes and a path is not, so the tag is an
            // ordinal and the guest is told the mapping on the command line.
            tag: format!("share{index}"),
            path: std::path::PathBuf::from(path),
        })
        .collect::<Vec<_>>();

    // The guest is told where to mount each share, and roughly what time it
    // is. It has no real-time clock, so without the second one every TLS
    // handshake fails with a complaint about a certificate. Whole seconds,
    // read here before the kernel starts: the seed for init, which the agent
    // replaces with an answer it asks the VMM for and corrects for the trip
    // (`lighter_vmm::clock`) as soon as it runs.
    // `psi=0`: nothing in the guest reads pressure stall information (the
    // host's memory pressure is macOS's own), and its averaging work woke a
    // CPU every two seconds on an idle machine, on top of its accounting on
    // every context switch.
    let mut cmdline = String::from(
        "console=ttyAMA0 panic=-1 root=/dev/vda rw init=/sbin/lighter-init reboot=t psi=0",
    );
    cmdline.push_str(&format!(
        " idle.poll_ns={}",
        crate::config::idle_poll_ns(config.cpus)
    ));
    cmdline.push_str(&format!(
        " lighter.time={}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    ));
    for share in &shares {
        cmdline.push_str(&format!(
            " lighter.share={}:{}",
            share.tag,
            share.path.display()
        ));
    }
    // Rosetta rides its own share, mounted by the guest's init at a fixed
    // place when told; without it amd64 containers fail naming the fix.
    let mut shares = shares;
    if lighter_vmm::rosetta::installed() {
        match lighter_vmm::rosetta::key() {
            Ok(_) => {
                shares.push(Share {
                    tag: lighter_vmm::rosetta::TAG.to_string(),
                    path: std::path::PathBuf::from(lighter_vmm::rosetta::DIR),
                });
                cmdline.push_str(" lighter.rosetta");
            }
            Err(e) => {
                tracing::warn!(%e, "Rosetta is installed but not usable; amd64 under emulation")
            }
        }
    }
    // The kernel join of a stream's two sockets: on unless `LIGHTER_SOCKMAP=0`,
    // which keeps the agent's copying path measurable.
    if std::env::var("LIGHTER_SOCKMAP")
        .map(|v| v == "0")
        .unwrap_or(false)
    {
        cmdline.push_str(" lighter.nosockmap");
    }
    // `LIGHTER_CMDLINE_EXTRA`: words appended to the guest's command line,
    // for an A/B of an agent or kernel knob on a machine run from the CLI
    // (the benchmark harness has the same).
    if let Ok(extra) = std::env::var("LIGHTER_CMDLINE_EXTRA") {
        let extra = extra.trim();
        if !extra.is_empty() {
            cmdline.push(' ');
            cmdline.push_str(extra);
        }
    }

    // The configured memory is the guest's maximum: it boots with a base
    // and plugs the rest in as the host offers it (`lighter_vmm::virtio::mem`).
    let (ram_bytes, hotplug_bytes) = lighter_vmm::virtio::mem::split(config.memory_mib << 20);
    let machine_config = MachineConfig {
        vcpus: config.cpus,
        ram_bytes,
        hotplug_bytes,
        kernel: paths::kernel()?,
        initramfs: None,
        cmdline,
        interactive: false,
        disks: vec![private_rootfs()?, paths::data_disk()?],
        disk_size_bytes: config.disk_gib << 30,
        network: true,
        run_dir: home.clone(),
        shares,
        // Rosetta asks the kernel for x86 ordering on its own threads.
        tso: false,
    };

    let mut machine = Machine::start(&machine_config)?;
    for (path, port) in machine::sockets()? {
        machine.proxy_socket(&path, port)?;
    }
    lighter_vmm::streams::start(machine.vsock())?;

    // Ports a container publishes appear on the Mac, for as long as the
    // container is running and no longer, through a stream into the guest.
    let scope = match config.publish {
        crate::config::Publish::Lan => lighter_vmm::streams::Scope::Lan,
        crate::config::Publish::Localhost => lighter_vmm::streams::Scope::Localhost,
    };
    let mapper = lighter_vmm::streams::PortMapper::new(machine.vsock(), scope);
    let ports = lighter_docker::PortWatcher::start(&paths::docker_socket()?, mapper)?;

    // A Mac that slept wakes with a guest whose clock did not.
    let _power = lighter_vmm::wake::Watcher::start(Box::new(Resync {
        woke: Arc::new(AtomicBool::new(false)),
    }));

    // SIGUSR1 is `lighter stop` saying the guest is about to be powered off:
    // the event stream into dockerd is dropped, so that dockerd, asked to
    // stop a moment later, does not sit out its five-second grace on it.
    // SIGTERM asks this process to power off its own guest. The caller
    // signals an audit token, so it cannot accidentally stop a replacement.
    install_signal_handler(ports)?;
    instance.publish()?;
    drop(selection_lease);

    let reason = machine.wait()?;
    tracing::info!(?reason, "machine stopped");
    Ok(())
}

use crate::machine;

/// Puts the guest's clock right after the Mac wakes.
struct Resync {
    woke: Arc<AtomicBool>,
}

impl Observer for Resync {
    fn power(&self, event: Power) {
        match event {
            Power::WillSleep => {
                self.woke.store(false, Ordering::Release);
                tracing::debug!("the host is going to sleep");
            }
            Power::Woke => resync_clock(),
        }
    }
}

/// Has the guest set its clock again, retrying while the agent comes back.
///
/// The same work `lighter resync` does, and deliberately the same code: a
/// recovery path that only runs when the lid opens is one nobody can test.
/// The time itself is not in the message: the agent asks the VMM for it and
/// corrects for the trip (`lighter_vmm::clock`), so a retry that lands five
/// seconds after this was called is as accurate as the first attempt. It
/// used to carry whole seconds read once, before the retries.
pub fn resync_clock() {
    // The agent may take a moment to be reachable after a wake, so this is
    // retried rather than attempted once.
    for attempt in 0..20 {
        match machine::control("time") {
            Ok(reply) if reply == "ok" => {
                tracing::info!(attempt, "guest clock resynchronised");
                return;
            }
            Ok(reply) => tracing::debug!(%reply, "guest declined the time"),
            Err(e) => tracing::debug!(%e, "guest not reachable yet"),
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    tracing::warn!("could not set the guest clock after waking");
}

/// The write end of the pipe SIGUSR1 pokes; a thread on the read end does
/// the work a handler may not.
static PREPARE_PIPE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Asks the machine to stop when the process is asked to.
fn install_signal_handler(ports: Arc<lighter_docker::http::Stop>) -> std::io::Result<()> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: storage for the two descriptors returned by pipe.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    // SAFETY: both descriptors were just created and each gets one owner.
    let reader = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    // A signal storm must not block inside its handler on a full pipe.
    if unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    std::thread::Builder::new()
        .name("prepare-stop".into())
        .spawn(move || {
            let mut byte = [0u8; 1];
            loop {
                // SAFETY: our pipe and one writable byte.
                let n = unsafe { libc::read(reader.as_raw_fd(), byte.as_mut_ptr().cast(), 1) };
                if n < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                if n != 1 {
                    break;
                }
                ports.stop();
                if STOP_REQUESTED.load(Ordering::Acquire) {
                    // This process still owns the home lock. Its control socket
                    // cannot belong to a replacement VM while this request runs.
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    let asked = machine::control("poweroff").is_ok_and(|reply| reply == "ok");
                    if !asked {
                        // No clean guest shutdown is possible. Exit successfully
                        // because this was an explicit stop, not a crash for
                        // launchd to restart.
                        unsafe { libc::_exit(0) };
                    }
                    break;
                }
            }
        })?;
    PREPARE_PIPE.store(writer.as_raw_fd(), Ordering::SeqCst);
    std::mem::forget(writer); // used by the handlers until process exit
    // SAFETY: handlers only set an atomic flag and perform a nonblocking write.
    unsafe {
        libc::signal(
            libc::SIGUSR1,
            handle_prepare_stop as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_stop as *const () as libc::sighandler_t,
        );
        libc::signal(libc::SIGINT, handle_stop as *const () as libc::sighandler_t);
    }
    Ok(())
}

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_prepare_stop(_signal: libc::c_int) {
    let fd = PREPARE_PIPE.load(Ordering::SeqCst);
    if fd >= 0 {
        // SAFETY: nonblocking write; an existing queued byte also wakes the reader.
        unsafe { libc::write(fd, b"s".as_ptr().cast(), 1) };
    }
}

extern "C" fn handle_stop(signal: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::Release);
    handle_prepare_stop(signal);
}
