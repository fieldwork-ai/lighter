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
const DOCKER_PORT: u32 = 2375;
const CONTROL_PORT: u32 = lighter_vmm::memory_policy::AGENT_CONTROL_PORT;

/// What `lighter status` found.
pub struct Status {
    pub pid: Option<u32>,
    pub running: bool,
    pub docker: Option<String>,
    pub footprint_mib: Option<u64>,
}

/// The process id in the pid file, if it names something alive.
pub fn running_pid() -> anyhow::Result<Option<u32>> {
    let path = paths::pid_file()?;
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return Ok(None);
    };
    // Signal zero asks whether we could signal it, without doing so — which is
    // the only way to tell a live process from a stale pid file.
    // SAFETY: a plain kill(2) with no side effect.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        Ok(Some(pid))
    } else {
        Ok(None)
    }
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
    // A socket left behind by a machine that was killed rather than stopped
    // would make the Docker CLI hang against nothing.
    let _ = std::fs::remove_file(&socket);

    // From the bundle, not from wherever the CLI binary happens to sit:
    // that is what gives the process a name and the flame in Activity
    // Monitor. The guest directory is resolved here and passed down,
    // because the bundled copy cannot find it by walking up from itself.
    let exe = crate::bundle::ensure()?;
    let guest = paths::guest_dir()?;
    let mut command = std::process::Command::new(exe);
    command.arg("run");
    command.env("LIGHTER_GUEST_DIR", &guest);
    let log_file = std::fs::File::create(&log)?;
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

    let child = command.spawn()?;
    let pid = child.id();
    std::fs::write(paths::pid_file()?, pid.to_string())?;

    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if docker_version(&socket).is_ok() {
            return Ok(pid);
        }
        // SAFETY: a plain kill(2) with no side effect.
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            anyhow::bail!(
                "the machine exited during start; see {}",
                paths::log_file()?.display()
            );
        }
        // Twenty milliseconds: the machine answers in about a second, and
        // the poll's granularity is inside every start a person times.
        std::thread::sleep(Duration::from_millis(20));
    }
    anyhow::bail!(
        "the machine did not answer within {}s; see {}",
        wait.as_secs(),
        paths::log_file()?.display()
    )
}

/// Asks the machine to stop, and waits for it.
pub fn stop(wait: Duration) -> anyhow::Result<bool> {
    let Some(pid) = running_pid()? else {
        let _ = std::fs::remove_file(paths::pid_file()?);
        return Ok(false);
    };
    // The guest first: its agent stops the engine, syncs and powers off, and
    // the machine process ends on its own. A machine killed from outside
    // loses whatever btrfs had not committed (it commits every 30 s): an
    // image pulled just before a stop was gone at the next start, "layer
    // does not exist". The signal is the fallback for a guest that does not
    // answer, and the kill the fallback for a machine that does not end.
    let asked = control("poweroff").map(|reply| reply == "ok").unwrap_or(false);
    if !asked {
        // SAFETY: a signal to a process we started.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }

    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if running_pid()?.is_none() {
            let _ = std::fs::remove_file(paths::pid_file()?);
            let _ = std::fs::remove_file(paths::docker_socket()?);
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // It has had its chance. A guest that will not shut down cleanly is not a
    // reason to leave a VM running forever.
    // SAFETY: as above.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    std::thread::sleep(Duration::from_millis(200));
    // SAFETY: as above.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    let _ = std::fs::remove_file(paths::pid_file()?);
    let _ = std::fs::remove_file(paths::docker_socket()?);
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
    let value =
        lighter_docker::http::get_json(socket, "/version").map_err(|e| anyhow::anyhow!("{e}"))?;
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
    stream.write_all(format!("{command}\n").as_bytes())?;
    // One line, not to end of file. The agent answers and keeps the connection
    // open for another command, so reading to EOF waits for something that is
    // never coming and fails with EAGAIN when the timeout expires.
    let mut reply = String::new();
    std::io::BufReader::new(&stream).read_line(&mut reply)?;
    Ok(reply.trim().to_string())
}

/// The machine's physical footprint, as macOS accounts it.
fn footprint_mib(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let kib: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    Some(kib / 1024)
}

/// The vsock ports the machine serves, and where they appear on the Mac.
pub fn sockets() -> anyhow::Result<Vec<(std::path::PathBuf, u32)>> {
    Ok(vec![
        (paths::docker_socket()?, DOCKER_PORT),
        (paths::home()?.join("control.sock"), CONTROL_PORT),
    ])
}
