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
    let current = {
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

    // Exactly one machine per home directory, enforced with a lock held for
    // the life of the process. Without this, `lighter install` while a
    // machine was already running had launchd start a second one — which
    // unlinked the first one's network sockets while failing to start, on a
    // KeepAlive loop, every few seconds. The guest kept its established
    // connections, so the breakage was maximally confusing: image pulls
    // worked and every new port forward died. The second copy exits
    // SUCCESSFULLY on purpose: launchd only restarts an unsuccessful exit,
    // so answering "already running" politely is what ends the loop.
    let lock_path = home.join("machine.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    // SAFETY: a valid descriptor, held (leaked) for the process lifetime.
    if unsafe {
        libc::flock(
            std::os::fd::AsRawFd::as_raw_fd(&lock),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    } != 0
    {
        eprintln!("lighter is already running; this copy has nothing to do");
        return Ok(());
    }
    std::mem::forget(lock);

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

    // The guest is told where to mount each share, and what time it is. It has
    // no real-time clock, so without the second one every TLS handshake fails
    // with a complaint about a certificate.
    let mut cmdline =
        String::from("console=ttyAMA0 panic=-1 root=/dev/vda rw init=/sbin/lighter-init reboot=t");
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

    let machine_config = MachineConfig {
        vcpus: config.cpus,
        ram_bytes: config.memory_mib << 20,
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
    let mapper = lighter_vmm::streams::PortMapper::new(machine.vsock());
    let ports = lighter_docker::PortWatcher::start(&paths::docker_socket()?, mapper)?;

    // A Mac that slept wakes with a guest whose clock did not.
    let _power = lighter_vmm::wake::Watcher::start(Box::new(Resync {
        woke: Arc::new(AtomicBool::new(false)),
    }));

    // SIGUSR1 is `lighter stop` saying the guest is about to be powered off:
    // the event stream into dockerd is dropped, so that dockerd, asked to
    // stop a moment later, does not sit out its five-second grace on it.
    // SIGTERM is the same command's fallback, and a machine that ignored it
    // would have to be killed — which is a guest that never unmounts its disk.
    install_signal_handler(ports);

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

/// Sets the guest's clock to now, retrying while the agent comes back.
///
/// The same work `lighter resync` does, and deliberately the same code: a
/// recovery path that only runs when the lid opens is one nobody can test.
pub fn resync_clock() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // The agent may take a moment to be reachable after a wake, so this is
    // retried rather than attempted once.
    for attempt in 0..20 {
        match machine::control(&format!("time {now}")) {
            Ok(reply) if reply == "ok" => {
                tracing::info!(epoch = now, attempt, "guest clock resynchronised");
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
fn install_signal_handler(ports: Arc<lighter_docker::http::Stop>) {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: a pipe for the self-pipe pattern; both ends are ours.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == 0 {
        PREPARE_PIPE.store(fds[1], Ordering::SeqCst);
        let read_end = fds[0];
        let _ = std::thread::Builder::new()
            .name("prepare-stop".into())
            .spawn(move || {
                let mut byte = [0u8; 1];
                // SAFETY: a blocking read on our own pipe.
                while unsafe { libc::read(read_end, byte.as_mut_ptr().cast(), 1) } == 1 {
                    ports.stop();
                    tracing::info!("stop announced; the port watcher let go of docker");
                }
            });
    }
    // SAFETY: installing handlers for SIGUSR1, SIGTERM and SIGINT. The
    // handlers do nothing but write a byte to a pipe, or exit — see below.
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
}

extern "C" fn handle_prepare_stop(_signal: libc::c_int) {
    let fd = PREPARE_PIPE.load(Ordering::SeqCst);
    if fd >= 0 {
        // SAFETY: `write` is async-signal-safe; one byte to our own pipe.
        unsafe { libc::write(fd, b"s".as_ptr().cast(), 1) };
    }
}

/// The whole handler: ask the process to end.
///
/// Deliberately not "stop the machine tidily". A signal handler may call
/// almost nothing, and a VM shutdown involves locks, threads and the
/// hypervisor. `_exit` drops the machine the way a crash would — which the
/// guest survives, because its disk is journalled and the one thing that must
/// not be lost, a container's `fsync`, has already reached the Mac by then.
extern "C" fn handle_stop(_signal: libc::c_int) {
    // SAFETY: `_exit` is async-signal-safe, which is the whole reason it is
    // what this handler does.
    unsafe { libc::_exit(0) };
}
