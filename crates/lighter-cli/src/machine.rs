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
}

/// The daemon that owns this home, verified with its process generation.
pub fn running_pid() -> anyhow::Result<Option<u32>> {
    Ok(crate::instance::Identity::read(&paths::home()?)?.map(|id| id.pid()))
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
    let exe = crate::bundle::ensure()?;
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

    let mut child = command.spawn()?;
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
        if let Some(status) = child.try_wait()? {
            anyhow::bail!(
                "the machine exited during start ({status}); see {}",
                log.display()
            );
        }
        std::thread::sleep(
            Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
        );
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
        return Ok(false);
    };
    // Signal the recorded process generation, not just its PID. The daemon
    // asks its own guest to sync/power off while it still holds the home lock.
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
    let docker = docker_version(&socket).ok();
    let footprint = pid.and_then(footprint_mib);
    Ok(Status {
        running: pid.is_some(),
        pid,
        docker,
        footprint_mib: footprint,
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
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(format!("{command}\n").as_bytes())?;
    // One line, not to end of file. The agent answers and keeps the connection
    // open for another command, so reading to EOF waits for something that is
    // never coming and fails with EAGAIN when the timeout expires.
    let mut reply = String::new();
    std::io::BufReader::new(&stream).read_line(&mut reply)?;
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
