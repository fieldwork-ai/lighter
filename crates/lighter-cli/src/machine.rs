//! Starting, stopping and inspecting the machine.
//!
//! The machine runs as its own process, and `lighter start` is a thing that
//! *spawns* one rather than a thing that becomes one. That split is what lets
//! the shell that started it exit, what lets launchd own it instead, and what
//! makes `lighter status` a question anyone can ask rather than something only
//! the terminal holding the VM knows.

use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use std::time::{Duration, Instant};

use crate::config::Config;
use crate::paths;

/// Guest vsock ports. The agent binds both.
const DOCKER_PORT: u32 = lighter_vmm::machine::DOCKER_PORT;
const CONTROL_PORT: u32 = lighter_vmm::memory_policy::AGENT_CONTROL_PORT;

/// What `lighter status` found.
pub struct Status {
    pub pid: Option<u32>,
    pub running: bool,
    pub docker: Option<String>,
    pub footprint_mib: Option<u64>,
    pub storage_waiting: Vec<crate::storage_status::Waiting>,
}

/// The daemon that owns this home, verified with its process generation;
/// or a machine started by a lighter before 0.4.0, which holds the home's
/// lock and wrote only a pid file.
pub fn running_pid() -> anyhow::Result<Option<u32>> {
    if let Some(identity) = crate::instance::Identity::read(&paths::home()?)? {
        return Ok(Some(identity.pid()));
    }
    Ok(legacy_machine(&paths::home()?)?.map(|machine| machine.identity.pid()))
}

struct LegacyMachine {
    identity: crate::instance::Identity,
    control: UnixStream,
}

/// The PID file is only a hint. Authenticate the process serving this home's
/// control socket and retain that connection across shutdown: reconnecting
/// could send poweroff to a successor after the old daemon exits.
fn legacy_machine(home: &Path) -> anyhow::Result<Option<LegacyMachine>> {
    let Ok(text) = std::fs::read_to_string(home.join("lighter.pid")) else {
        return Ok(None);
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return Ok(None);
    };
    if pid == 0 || pid > i32::MAX as u32 {
        return Ok(None);
    }
    let Ok(lock) = std::fs::File::open(home.join("machine.lock")) else {
        return Ok(None);
    };
    if crate::instance::try_lock(&lock)? {
        return Ok(None);
    }
    // Fail explicitly if a locked legacy home cannot be authenticated. Never
    // fall back to kill(pid), even when the socket is unavailable.
    let control = UnixStream::connect(home.join("control.sock"))?;
    let identity = crate::instance::Identity::peer(home, &control)?;
    if identity.pid() != pid {
        anyhow::bail!("legacy PID file does not match the control socket owner");
    }
    if identity
        .executable()?
        .file_name()
        .is_none_or(|name| name != "lighter")
    {
        anyhow::bail!("legacy control socket owner is not a lighter executable");
    }
    Ok(Some(LegacyMachine { identity, control }))
}

fn stop_legacy(mut machine: LegacyMachine, wait: Duration) -> anyhow::Result<bool> {
    let identity = &machine.identity;
    if !identity.signal(libc::SIGUSR1)? {
        return Ok(false);
    }
    std::thread::sleep(Duration::from_millis(150));
    let asked = control_on(&mut machine.control, "poweroff")
        .map(|reply| reply == "ok")
        .unwrap_or(false);
    if !asked {
        identity.signal(libc::SIGTERM)?;
    }
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline && identity.alive()? {
        std::thread::sleep(Duration::from_millis(100));
    }
    if identity.alive()? {
        identity.signal(libc::SIGKILL)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while identity.alive()? {
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "legacy machine {} has not exited after SIGKILL",
                    identity.pid()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    // Leave files alone. A successor may already own this home. The next
    // daemon clears stale PID state and socket paths under its lifetime lock.
    Ok(true)
}

/// Starts a machine and waits for Docker to answer.
pub fn start(_config: &Config, wait: Duration) -> anyhow::Result<u32> {
    if let Some(pid) = running_pid()? {
        anyhow::bail!("lighter is already running (pid {pid})");
    }

    let home = paths::home()?;
    std::fs::create_dir_all(&home)?;
    let socket = paths::docker_socket()?;
    let log = paths::log_file()?;
    // From the bundle, not from wherever the CLI binary happens to sit:
    // that is what gives the process a name and the flame in Activity
    // Monitor. The guest directory is resolved here and passed down,
    // because the bundled copy cannot find it by walking up from itself.
    let bundle_phase = lighter_vmm::boot_timing::Phase::new("cli_bundle");
    let exe = crate::bundle::ensure()?;
    drop(bundle_phase);
    let guest = paths::guest_dir()?;
    let mut command = std::process::Command::new(exe);
    command.arg("run");
    command.env("LIGHTER_GUEST_DIR", &guest);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file.try_clone()?))
        .stderr(std::process::Stdio::from(log_file));

    // A new session, so the machine is not killed when the terminal that
    // started it goes away.
    // SAFETY: `setsid` in the child between fork and exec, which is
    // async-signal-safe and is exactly what this hook is for.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let spawn_phase = lighter_vmm::boot_timing::Phase::new("cli_spawn");
    let mut child = if crate::service::start_registered()? {
        None
    } else {
        Some(command.spawn()?)
    };
    drop(spawn_phase);
    let _readiness = lighter_vmm::boot_timing::Phase::new("cli_docker_readiness");
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if let Some(identity) = crate::instance::Identity::read(&home)?
            && docker_version_until(
                &socket,
                deadline.min(Instant::now() + Duration::from_secs(1)),
            )
            .is_ok()
            && identity.alive()?
        {
            // launchd may have won the lock; return the actual owner, never
            // publish the PID of the losing child into its state directory.
            return Ok(identity.pid());
        }
        if let Some(child) = child.as_mut()
            && let Some(status) = child.try_wait()?
        {
            anyhow::bail!(
                "the machine exited during start ({status}); see {}",
                log.display()
            );
        }
        std::thread::sleep(
            Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    if let Some(child) = child.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    anyhow::bail!(
        "the machine did not answer within {}s; see {}",
        wait.as_secs(),
        paths::log_file()?.display()
    )
}

/// Asks the machine to stop, and waits for it.
pub fn stop(wait: Duration) -> anyhow::Result<bool> {
    let Some(identity) = crate::instance::Identity::read(&paths::home()?)? else {
        return match legacy_machine(&paths::home()?)? {
            Some(machine) => stop_legacy(machine, wait),
            None => Ok(false),
        };
    };
    // Signal the recorded process generation, not just its PID. The daemon
    // asks its own guest to sync/power off while it still holds the home lock.
    if crate::storage_status::query(&paths::home()?, identity.pid()).is_ok_and(|s| !s.is_empty()) {
        eprintln!(
            "Storage is waiting for host disk space. Stopping now may lose unfinished writes."
        );
    }
    if !identity.signal(libc::SIGTERM)? {
        return Ok(false);
    }
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if !identity.alive()? {
            return Ok(true);
        }
        std::thread::sleep(
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    identity.signal(libc::SIGKILL)?;
    let killed_by = Instant::now() + Duration::from_secs(5);
    while identity.alive()? {
        if Instant::now() >= killed_by {
            anyhow::bail!("machine {} has not exited after SIGKILL", identity.pid());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Never unlink state from this process: launchd may already have started
    // a successor. The owner cleans up, or the next owner does under its lock.
    Ok(true)
}

/// Everything `lighter status` reports.
pub fn status() -> anyhow::Result<Status> {
    let pid = running_pid()?;
    let socket = paths::docker_socket()?;
    let storage_waiting = pid
        .and_then(|pid| crate::storage_status::query(&paths::home().ok()?, pid).ok())
        .unwrap_or_default();
    let timeout = if storage_waiting.is_empty() {
        Duration::from_secs(5)
    } else {
        Duration::from_millis(250)
    };
    let docker = docker_version_until(&socket, Instant::now() + timeout).ok();
    let footprint = pid.and_then(footprint_mib);
    Ok(Status {
        running: pid.is_some(),
        pid,
        docker,
        footprint_mib: footprint,
        storage_waiting,
    })
}

/// The daemon's version string, which doubles as "is it answering".
///
/// Through the crate that already speaks this protocol rather than a second
/// hand-rolled client. The first version here spoke HTTP/1.0, which dockerd
/// answers with a 500 — a detail worth exactly one discovery.
pub fn docker_version(socket: &Path) -> anyhow::Result<String> {
    docker_version_until(socket, Instant::now() + Duration::from_secs(5))
}

fn docker_version_until(socket: &Path, deadline: Instant) -> anyhow::Result<String> {
    let value = lighter_docker::http::get_json_until(socket, "/version", deadline)?;
    let version = value
        .get("Version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("the daemon answered without a version"))?;
    let os = value.get("Os").and_then(|v| v.as_str()).unwrap_or("linux");
    let arch = value.get("Arch").and_then(|v| v.as_str()).unwrap_or("");
    Ok(format!("{version} on {os}/{arch}"))
}

/// Sends one line to the guest's control port and returns its answer.
pub fn control(command: &str) -> anyhow::Result<String> {
    let socket = paths::home()?.join("control.sock");
    let mut stream = UnixStream::connect(&socket)?;
    control_on(&mut stream, command)
}

fn control_on(stream: &mut UnixStream, command: &str) -> anyhow::Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(format!("{command}\n").as_bytes())?;
    // One line, not to end of file. The agent answers and keeps the connection
    // open for another command, so reading to EOF waits for something that is
    // never coming and fails with EAGAIN when the timeout expires.
    let mut reply = String::new();
    std::io::BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply.trim().to_string())
}

/// The machine's physical footprint, as macOS accounts it: `phys_footprint`,
/// the number Activity Monitor shows and memory pressure is decided from.
/// The resident set size read twice that on a machine running a day's
/// stack (2.7 GB against 1.3), since it counts the guest's pages the Mac
/// has already taken back.
fn footprint_mib(pid: u32) -> Option<u64> {
    // SAFETY: `rusage_info_v2` is plain data of the size the kernel is told
    // to fill, and the pid is a process this user owns.
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::uninit();
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V2,
            info.as_mut_ptr() as *mut libc::rusage_info_t,
        )
    };
    if rc != 0 {
        return None;
    }
    // SAFETY: the call succeeded, so the structure is filled.
    let info = unsafe { info.assume_init() };
    Some(info.ri_phys_footprint >> 20)
}

/// The vsock ports the machine serves, and where they appear on the Mac.
pub fn sockets() -> anyhow::Result<Vec<(std::path::PathBuf, u32)>> {
    Ok(vec![
        (paths::docker_socket()?, DOCKER_PORT),
        (paths::home()?.join("control.sock"), CONTROL_PORT),
    ])
}

#[cfg(test)]
mod legacy_tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn legacy_child() {
        let Some(home) = std::env::var_os("LIGHTER_TEST_LEGACY_HOME") else {
            return;
        };
        let home = Path::new(&home);
        let _owner = crate::instance::Instance::acquire(home).unwrap().unwrap();
        // A pre-identity daemon only publishes a numeric PID.
        std::fs::write(home.join("lighter.pid"), std::process::id().to_string()).unwrap();
        // SAFETY: the test daemon needs the old watcher's SIGUSR1 semantics.
        unsafe {
            libc::signal(libc::SIGUSR1, libc::SIG_IGN);
        }
        let listener = UnixListener::bind(home.join("control.sock")).unwrap();
        for connection in listener.incoming() {
            let mut stream = connection.unwrap();
            let mut line = String::new();
            if std::io::BufReader::new(&stream)
                .read_line(&mut line)
                .unwrap()
                == 0
            {
                continue;
            }
            assert_eq!(line, "poweroff\n");
            stream.write_all(b"ok\n").unwrap();
            break;
        }
    }

    #[test]
    fn legacy_peer_is_authenticated_and_shutdown_keeps_replacement_paths() {
        let home = std::env::temp_dir().join(format!("lighter-legacy-{}", std::process::id()));
        std::fs::create_dir(&home).unwrap();
        struct Fixture {
            home: std::path::PathBuf,
            child: std::process::Child,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                let _ = std::fs::remove_dir_all(&self.home);
            }
        }
        // proc_pidpath must report the shipped executable's basename.
        let exe = home.join("lighter");
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        let child = std::process::Command::new(&exe)
            .args(["--exact", "machine::legacy_tests::legacy_child"])
            .env("LIGHTER_TEST_LEGACY_HOME", &home)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut fixture = Fixture {
            home: home.clone(),
            child,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !home.join("control.sock").exists() {
            assert!(Instant::now() < deadline, "legacy daemon did not bind");
            assert!(fixture.child.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(10));
        }
        // A held home lock cannot make an unrelated PID authoritative.
        std::fs::write(home.join("lighter.pid"), std::process::id().to_string()).unwrap();
        assert!(legacy_machine(&home).is_err());
        assert!(fixture.child.try_wait().unwrap().is_none());
        std::fs::write(home.join("lighter.pid"), fixture.child.id().to_string()).unwrap();
        let machine = legacy_machine(&home).unwrap().unwrap();
        assert_eq!(machine.identity.pid(), fixture.child.id());
        // Replace the endpoint after authentication. Shutdown must use the
        // original connection and must never unlink the replacement's state.
        std::fs::remove_file(home.join("control.sock")).unwrap();
        let replacement = UnixListener::bind(home.join("control.sock")).unwrap();
        replacement.set_nonblocking(true).unwrap();
        let docker = UnixListener::bind(home.join("docker.sock")).unwrap();
        std::fs::write(home.join("lighter.pid"), "replacement").unwrap();
        assert!(stop_legacy(machine, Duration::from_secs(2)).unwrap());
        assert!(fixture.child.wait().unwrap().success());
        assert!(home.join("control.sock").exists());
        assert!(home.join("docker.sock").exists());
        assert_eq!(
            std::fs::read_to_string(home.join("lighter.pid")).unwrap(),
            "replacement"
        );
        assert_eq!(
            replacement.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop((replacement, docker));
    }
}
