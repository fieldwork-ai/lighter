//! On-demand storage status, independent of the guest and host free space.
use lighter_vmm::virtio::disk::Disk;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

const SOCKET: &str = "status.sock";

#[derive(Debug, Serialize, Deserialize)]
pub struct Waiting {
    pub disk: PathBuf,
    pub operation: String,
    pub waiting_seconds: u64,
    pub retries: u64,
}

/// The guest's memory, when it has a virtio-mem range (cooperative resources).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Memory {
    /// What the guest booted with.
    pub base_mib: u64,
    /// How much of the range the guest has plugged in.
    pub plugged_mib: u64,
    /// The range's size: base and range together are the ceiling.
    pub range_mib: u64,
}

/// What the machine reports on demand.
#[derive(Debug, Default)]
pub struct Report {
    pub waiting: Vec<Waiting>,
    pub memory: Option<Memory>,
}

#[derive(Serialize, Deserialize)]
struct Reply {
    version: u32,
    waiting: Vec<Waiting>,
    #[serde(default)]
    memory: Option<Memory>,
}

pub struct Server {
    path: PathBuf,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Caller holds the instance lock for this home throughout our lifetime.
    pub fn start(
        home: &Path,
        disks: Vec<(PathBuf, Arc<Disk>)>,
        mem: Option<lighter_vmm::virtio::mem::MemControl>,
    ) -> io::Result<Self> {
        let path = home.join(SOCKET);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let thread = std::thread::Builder::new()
            .name("storage-status".into())
            .spawn(move || {
                for connection in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(mut stream) = connection else {
                        break;
                    };
                    if stream
                        .set_write_timeout(Some(Duration::from_millis(250)))
                        .is_err()
                    {
                        continue;
                    }
                    let waiting = disks
                        .iter()
                        .filter_map(|(path, disk)| {
                            disk.waiting().map(|s| Waiting {
                                disk: path.clone(),
                                operation: s.operation.into(),
                                waiting_seconds: s.since.elapsed().as_secs(),
                                retries: s.retries,
                            })
                        })
                        .collect();
                    let memory = mem.as_ref().map(|mem| {
                        let state = mem.state();
                        Memory {
                            base_mib: state.base_bytes() >> 20,
                            plugged_mib: state.plugged_bytes() >> 20,
                            range_mib: state.region_bytes() >> 20,
                        }
                    });
                    let reply = Reply {
                        version: 1,
                        waiting,
                        memory,
                    };
                    if let Ok(bytes) = serde_json::to_vec(&reply) {
                        let _ = stream.write_all(&bytes);
                    }
                }
            })?;
        Ok(Self {
            path,
            stopped,
            thread: Some(thread),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        // accept() sleeps indefinitely in the happy case; no polling thread.
        let _ = UnixStream::connect(&self.path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn query(home: &Path, pid: u32) -> io::Result<Report> {
    let mut stream = UnixStream::connect(home.join(SOCKET))?;
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    if crate::instance::Identity::peer(home, &stream)?.pid() != pid {
        return Err(io::Error::other(
            "storage status belongs to a different daemon",
        ));
    }
    let mut bytes = Vec::new();
    (&mut stream).take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(io::Error::other("storage status too large"));
    }
    let reply: Reply = serde_json::from_slice(&bytes)?;
    if reply.version != 1 {
        return Err(io::Error::other("unknown storage status version"));
    }
    Ok(Report {
        waiting: reply.waiting,
        memory: reply.memory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_on_demand_authenticated_and_removed_on_shutdown() {
        let home =
            std::env::temp_dir().join(format!("lighter-storage-status-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let server = Server::start(&home, Vec::new(), None).unwrap();
        let report = query(&home, std::process::id()).unwrap();
        assert!(report.waiting.is_empty());
        assert!(report.memory.is_none(), "no range, no memory report");
        assert!(query(&home, std::process::id() + 1).is_err());
        assert_eq!(
            std::fs::metadata(home.join(SOCKET))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(server);
        assert!(!home.join(SOCKET).exists());
        assert_eq!(
            query(&home, std::process::id()).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        std::fs::remove_dir(home).unwrap();
    }
}
