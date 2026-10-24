//! Starting the machine as a process responsible for itself.
//!
//! macOS asks privacy questions, Local Network among them, of the process
//! responsible for the one asking, and a process started from a shell is the
//! responsibility of the app the shell runs in: Terminal, an editor, an
//! agent. A machine `lighter start` spawned plainly had that app's Local
//! Network permission rather than its own. It reached the network from
//! Terminal, which had been allowed, and nothing from an app that had not;
//! and it lost the network when the app that started it quit (#101). The
//! same `lighter.app`, started the same way, behaved three ways depending
//! on where `lighter start` was typed.
//!
//! So the machine disclaims that responsibility when it is spawned, as
//! launchd's own children do, and is asked about as `lighter.app` wherever
//! it was started from. `responsibility_spawnattrs_setdisclaim` is what
//! Chromium and LLDB use for the same reason; it is not in Apple's headers,
//! so it is looked up when needed, and a Mac without it starts the machine
//! the way it always did.

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;

// `sys/spawn.h`; the libc crate has the second and not the first.
const POSIX_SPAWN_SETSID: libc::c_short = 0x0400;
const POSIX_SPAWN_CLOEXEC_DEFAULT: libc::c_short = libc::POSIX_SPAWN_CLOEXEC_DEFAULT as _;

/// A process spawned by [`spawn`], waited for the way `std::process::Child` is.
pub struct Child {
    pid: libc::pid_t,
    status: Option<ExitStatus>,
}

impl Child {
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.reap(libc::WNOHANG)
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.reap(0)
            .map(|status| status.expect("waitpid without WNOHANG returns a status"))
    }

    pub fn kill(&mut self) -> io::Result<()> {
        if self.status.is_some() {
            return Ok(());
        }
        // SAFETY: our own unreaped child, so the pid cannot have been reused.
        if unsafe { libc::kill(self.pid, libc::SIGKILL) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn reap(&mut self, flags: libc::c_int) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let mut status = 0;
        loop {
            // SAFETY: our own child, and a valid out-pointer.
            match unsafe { libc::waitpid(self.pid, &mut status, flags) } {
                0 => return Ok(None),
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                -1 => return Err(io::Error::last_os_error()),
                _ => {
                    let status = ExitStatus::from_raw(status);
                    self.status = Some(status);
                    return Ok(Some(status));
                }
            }
        }
    }
}

/// Spawns `program` with `args` in a session of its own, responsible for
/// itself, with stdin from `/dev/null`, stdout and stderr to `output`, the
/// environment plus `env`, and no other descriptor of ours.
pub fn spawn(
    program: &Path,
    args: &[&OsStr],
    env: &[(&str, &OsStr)],
    output: &std::fs::File,
) -> io::Result<Child> {
    let cstring = |s: &OsStr| {
        CString::new(s.as_bytes()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    };
    let path = cstring(program.as_os_str())?;
    let argv = std::iter::once(program.as_os_str())
        .chain(args.iter().copied())
        .map(cstring)
        .collect::<io::Result<Vec<_>>>()?;
    let mut environment: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(key, _)| !env.iter().any(|(name, _)| key == name))
        .collect();
    environment.extend(env.iter().map(|(name, value)| (name.into(), value.into())));
    let envp = environment
        .iter()
        .map(|(key, value)| {
            let mut pair = key.clone();
            pair.push("=");
            pair.push(value);
            cstring(&pair)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let pointers = |strings: &[CString]| {
        strings
            .iter()
            .map(|s| s.as_ptr().cast_mut())
            .chain(std::iter::once(std::ptr::null_mut()))
            .collect::<Vec<_>>()
    };
    let (argv, envp) = (pointers(&argv), pointers(&envp));

    let mut attributes = Attributes::new()?;
    let mut actions = FileActions::new()?;
    // SAFETY: initialised attributes and actions, valid descriptors and
    // NUL-terminated strings that outlive the spawn.
    unsafe {
        check(libc::posix_spawnattr_setflags(
            &mut attributes.0,
            POSIX_SPAWN_SETSID
                | POSIX_SPAWN_CLOEXEC_DEFAULT
                | libc::POSIX_SPAWN_SETSIGMASK as libc::c_short
                | libc::POSIX_SPAWN_SETSIGDEF as libc::c_short,
        ))?;
        // What `std::process::Command` gives a child: no signal blocked, and
        // SIGPIPE back to its default, since Rust's runtime ignores it here
        // and an ignored signal stays ignored across an exec.
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        check(libc::posix_spawnattr_setsigmask(&mut attributes.0, &none))?;
        let mut pipe: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut pipe);
        libc::sigaddset(&mut pipe, libc::SIGPIPE);
        check(libc::posix_spawnattr_setsigdefault(
            &mut attributes.0,
            &pipe,
        ))?;
        disclaim(&mut attributes.0);
        check(libc::posix_spawn_file_actions_addopen(
            &mut actions.0,
            0,
            c"/dev/null".as_ptr(),
            libc::O_RDONLY,
            0,
        ))?;
        check(libc::posix_spawn_file_actions_adddup2(
            &mut actions.0,
            output.as_raw_fd(),
            1,
        ))?;
        check(libc::posix_spawn_file_actions_adddup2(
            &mut actions.0,
            output.as_raw_fd(),
            2,
        ))?;
        let mut pid = 0;
        check(libc::posix_spawn(
            &mut pid,
            path.as_ptr(),
            &actions.0,
            &attributes.0,
            argv.as_ptr(),
            envp.as_ptr(),
        ))?;
        Ok(Child { pid, status: None })
    }
}

/// Marks the spawn as disclaiming responsibility, where macOS has the call.
///
/// # Safety
/// `attributes` must be initialised.
unsafe fn disclaim(attributes: &mut libc::posix_spawnattr_t) {
    type Disclaim = unsafe extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int;
    // SAFETY: a NUL-terminated name, looked up in the images already loaded.
    let symbol = unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            c"responsibility_spawnattrs_setdisclaim".as_ptr(),
        )
    };
    if symbol.is_null() {
        tracing::debug!(
            "no responsibility_spawnattrs_setdisclaim; the machine shares its starter's permissions"
        );
        return;
    }
    // SAFETY: the symbol has this signature on every macOS that exports it.
    let disclaim: Disclaim = unsafe { std::mem::transmute(symbol) };
    // SAFETY: initialised attributes, by the caller's contract.
    if let Err(e) = check(unsafe { disclaim(attributes, 1) }) {
        tracing::debug!(%e, "could not disclaim responsibility for the machine");
    }
}

fn check(code: libc::c_int) -> io::Result<()> {
    match code {
        0 => Ok(()),
        errno => Err(io::Error::from_raw_os_error(errno)),
    }
}

struct Attributes(libc::posix_spawnattr_t);

impl Attributes {
    fn new() -> io::Result<Self> {
        let mut attributes = std::ptr::null_mut();
        // SAFETY: an out-pointer for the initialiser to fill.
        check(unsafe { libc::posix_spawnattr_init(&mut attributes) })?;
        Ok(Self(attributes))
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: initialised in `new`, destroyed once.
        unsafe { libc::posix_spawnattr_destroy(&mut self.0) };
    }
}

struct FileActions(libc::posix_spawn_file_actions_t);

impl FileActions {
    fn new() -> io::Result<Self> {
        let mut actions = std::ptr::null_mut();
        // SAFETY: an out-pointer for the initialiser to fill.
        check(unsafe { libc::posix_spawn_file_actions_init(&mut actions) })?;
        Ok(Self(actions))
    }
}

impl Drop for FileActions {
    fn drop(&mut self) {
        // SAFETY: initialised in `new`, destroyed once.
        unsafe { libc::posix_spawn_file_actions_destroy(&mut self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::spawn;
    use std::ffi::OsStr;
    use std::os::fd::AsRawFd;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn sh(script: &str, env: &[(&str, &OsStr)]) -> (super::Child, tempfile::NamedTempFile) {
        let output = tempfile::NamedTempFile::new().unwrap();
        let child = spawn(
            Path::new("/bin/sh"),
            &[OsStr::new("-c"), OsStr::new(script)],
            env,
            output.as_file(),
        )
        .unwrap();
        (child, output)
    }

    fn read(output: &tempfile::NamedTempFile) -> String {
        std::fs::read_to_string(output.path()).unwrap()
    }

    #[test]
    fn output_goes_to_the_file_with_the_environment_given() {
        let (mut child, output) = sh(
            "printf '%s|' \"$LIGHTER_SPAWN_TEST\" \"$HOME\"; printf err >&2",
            &[("LIGHTER_SPAWN_TEST", OsStr::new("given"))],
        );
        assert!(child.wait().unwrap().success());
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(read(&output), format!("given|{home}|err"));
    }

    #[test]
    fn the_exit_status_is_seen_without_blocking() {
        let (mut child, _output) = sh("exit 3", &[]);
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "the child never exited");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(status.code(), Some(3));
        // Reaped once, remembered after.
        assert_eq!(child.wait().unwrap().code(), Some(3));
    }

    #[test]
    fn the_child_leads_a_session_of_its_own() {
        // A session leader is its own process group, whatever the parent's is.
        let (mut child, output) = sh("ps -o pgid= -p $$", &[]);
        assert!(child.wait().unwrap().success());
        assert_eq!(read(&output).trim(), child.pid.to_string());
    }

    #[test]
    fn no_other_descriptor_is_inherited() {
        let file = tempfile::tempfile().unwrap();
        // SAFETY: a valid descriptor, duplicated above anything sh opens, and
        // without close-on-exec, which is what a plain spawn would hand on.
        let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 90) };
        assert!(fd >= 90);
        let (mut child, output) = sh(
            &format!("[ -e /dev/fd/{fd} ] && echo open || echo closed"),
            &[],
        );
        assert!(child.wait().unwrap().success());
        // SAFETY: ours, from the dup above.
        unsafe { libc::close(fd) };
        assert_eq!(read(&output).trim(), "closed");
    }

    #[test]
    fn stdin_is_empty() {
        let (mut child, output) = sh("wc -c | tr -d ' '", &[]);
        assert!(child.wait().unwrap().success());
        assert_eq!(read(&output).trim(), "0");
    }

    #[test]
    fn a_killed_child_reports_the_signal() {
        let (mut child, _output) = sh("sleep 60", &[]);
        child.kill().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }
}
