//! The identity and lifetime lock of one machine home.
//!
//! A PID is only a hint for shell tools. Commands use the daemon's audit token:
//! its pidversion distinguishes a recycled PID, and macOS checks it as part of
//! signaling, rather than in a racy check followed by kill(2).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

unsafe extern "C" {
    // SDK: libproc.h. Returns an errno value, not -1/errno.
    fn proc_pidpath_audittoken(
        token: *const [u32; 8],
        buffer: *mut libc::c_void,
        size: u32,
    ) -> libc::c_int;
    fn proc_signal_with_audittoken(token: *const [u32; 8], signal: libc::c_int) -> libc::c_int;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    token: [u32; 8],
    home: PathBuf,
    device: u64,
    inode: u64,
}

impl Identity {
    #[allow(deprecated)] // libc provides the Mach task port; no extra wrapper crate needed.
    fn current(home: &Path) -> io::Result<Self> {
        let home = home.canonicalize()?;
        let meta = home.metadata()?;
        let mut token = [0u32; 8];
        let mut count = 8;
        // SAFETY: TASK_AUDIT_TOKEN (15) fills eight 32-bit words owned here.
        let rc = unsafe {
            libc::task_info(
                libc::mach_task_self_,
                15,
                token.as_mut_ptr().cast(),
                &mut count,
            )
        };
        if rc != 0 || count != 8 {
            return Err(io::Error::other(format!(
                "cannot read process audit token: {rc}"
            )));
        }
        Ok(Self {
            token,
            home,
            device: meta.dev(),
            inode: meta.ino(),
        })
    }

    pub fn pid(&self) -> u32 {
        self.token[5]
    }

    /// The kernel matches PID AND pidversion, including on the final SIGKILL.
    pub fn signal(&self, signal: libc::c_int) -> io::Result<bool> {
        if self.pid() == 0 || self.pid() > i32::MAX as u32 {
            return Ok(false);
        }
        // SAFETY: a complete audit token. No task ports or elevated rights are
        // used; the kernel also applies the ordinary signal permission checks.
        match unsafe { proc_signal_with_audittoken(&self.token, signal) } {
            0 => Ok(true),
            libc::ESRCH => Ok(false),
            errno => Err(io::Error::from_raw_os_error(errno)),
        }
    }

    pub fn alive(&self) -> io::Result<bool> {
        let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // The audit-token signal API does not accept signal zero. Its path
        // query does the same PID/pidversion lookup without sending a signal.
        // SAFETY: a complete token and a correctly sized output buffer.
        let n = unsafe {
            proc_pidpath_audittoken(&self.token, path.as_mut_ptr().cast(), path.len() as u32)
        };
        if n > 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH | libc::ENOENT) => Ok(false),
            _ => Err(error),
        }
    }

    pub fn read(home: &Path) -> io::Result<Option<Self>> {
        let bytes = match std::fs::read(home.join("machine.identity")) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let Ok(identity) = serde_json::from_slice::<Self>(&bytes) else {
            return Ok(None);
        };
        let home = home.canonicalize()?;
        let meta = home.metadata()?;
        if identity.home != home || identity.device != meta.dev() || identity.inode != meta.ino() {
            return Ok(None);
        }
        // A record is only authoritative while the daemon holds the home lock.
        let lock = OpenOptions::new()
            .read(true)
            .open(home.join("machine.lock"))?;
        if try_lock(&lock)? || !identity.alive()? {
            return Ok(None);
        }
        Ok(Some(identity))
    }
}

pub(crate) fn try_lock(file: &File) -> io::Result<bool> {
    // SAFETY: a live descriptor. Dropping it releases a successfully taken lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(error)
    }
}

/// Held until all machine resources have gone away. Only this owner publishes
/// or removes state; a competing start never unlinks the running VM's sockets.
pub struct Instance {
    home: PathBuf,
    _lock: File,
    published: bool,
}

impl Instance {
    pub fn acquire(home: &Path) -> io::Result<Option<Self>> {
        std::fs::create_dir_all(home)?;
        let home = home.canonicalize()?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(home.join("machine.lock"))?;
        if !try_lock(&lock)? {
            return Ok(None);
        }
        // Stale files from a crash belong to this home. The lock makes their
        // removal safe; a parent CLI must never do this before spawning.
        for name in ["machine.identity", "lighter.pid"] {
            match std::fs::remove_file(home.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(Some(Self {
            home,
            _lock: lock,
            published: false,
        }))
    }

    /// Publish only after signal handling is installed, including on launchd.
    pub fn publish(&mut self) -> io::Result<()> {
        let identity = Identity::current(&self.home)?;
        let staging = self.home.join(".machine.identity.next");
        std::fs::write(&staging, serde_json::to_vec(&identity)?)?;
        std::fs::write(self.home.join("lighter.pid"), identity.pid().to_string())?;
        std::fs::rename(staging, self.home.join("machine.identity"))?;
        self.published = true;
        Ok(())
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if self.published {
            for name in [
                "machine.identity",
                "lighter.pid",
                "docker.sock",
                "control.sock",
            ] {
                let _ = std::fs::remove_file(self.home.join(name));
            }
        }
        // The lock is released after cleanup, so this cannot remove a successor.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    struct Home(PathBuf);
    impl Home {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "lighter-instance-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn owner_publishes_and_cleans_up_under_the_lock() {
        let home = Home::new();
        let mut owner = Instance::acquire(&home.0).unwrap().unwrap();
        owner.publish().unwrap();
        assert_eq!(
            Identity::read(&home.0).unwrap().unwrap().pid(),
            std::process::id()
        );
        assert!(Instance::acquire(&home.0).unwrap().is_none());
        assert!(home.0.join("machine.identity").exists());
        drop(owner);
        assert!(Identity::read(&home.0).unwrap().is_none());
        assert!(Instance::acquire(&home.0).unwrap().is_some());
    }

    #[test]
    fn a_record_copied_from_another_home_is_not_its_owner() {
        let a = Home::new();
        let b = Home::new();
        let mut owner = Instance::acquire(&a.0).unwrap().unwrap();
        let _other = Instance::acquire(&b.0).unwrap().unwrap();
        owner.publish().unwrap();
        std::fs::copy(a.0.join("machine.identity"), b.0.join("machine.identity")).unwrap();
        assert!(Identity::read(&b.0).unwrap().is_none());
    }

    #[test]
    fn a_recycled_pid_cannot_be_signalled() {
        let home = Home::new();
        let mut old = Identity::current(&home.0).unwrap();
        old.token[7] = old.token[7].wrapping_add(1); // same PID, different pidversion
        assert!(!old.signal(libc::SIGTERM).unwrap()); // must not terminate this test process
        assert!(!old.alive().unwrap());
    }

    #[test]
    fn legacy_pid_files_are_not_authority_to_signal() {
        let home = Home::new();
        std::fs::write(home.0.join("lighter.pid"), std::process::id().to_string()).unwrap();
        assert!(Identity::read(&home.0).unwrap().is_none());
    }

    #[test]
    fn daemon_child() {
        let Some(home) = std::env::var_os("LIGHTER_TEST_INSTANCE_CHILD") else {
            return;
        };
        let mut owner = Instance::acquire(Path::new(&home)).unwrap().unwrap();
        owner.publish().unwrap();
        std::thread::sleep(Duration::from_secs(15));
    }

    #[test]
    fn a_directly_started_owner_can_be_found_and_stopped() {
        let home = Home::new();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "instance::tests::daemon_child"])
            .env("LIGHTER_TEST_INSTANCE_CHILD", &home.0)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let identity = loop {
            if let Some(identity) = Identity::read(&home.0).unwrap() {
                break identity;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("child did not publish");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(identity.pid(), child.id());
        assert!(identity.signal(libc::SIGTERM).unwrap());
        child.wait().unwrap();
        assert!(!identity.alive().unwrap());
        assert!(Identity::read(&home.0).unwrap().is_none());
        assert!(Instance::acquire(&home.0).unwrap().is_some());
    }
}
