//! The directories a network volume is mounted in.
//!
//! A request names the directory it is about, and a LOOKUP names the parent
//! of what it looks up. So a lookup of `NAS` in `/Volumes`, or of a share
//! mounted in the home folder, is by its node a request on this Mac's own
//! disk, and was served on the vCPU that asked ([`crate::Server::may_block`]).
//! But the name is a mount point: the stat crosses into the network volume and
//! waits for its server. A NAS that stopped answering stopped that CPU, and
//! the device lock it held stopped every other vCPU that touched the share.
//! With the server inside the same machine (the m16 gate's Samba) the CPU it
//! stopped was one the server needed, and the machine waited out the Mac's
//! SMB timeout: ten minutes.
//!
//! A request on a directory that holds a network volume's mount point is
//! therefore one that may wait on a server. The directories are read from
//! the kernel's mount table, which answers without asking any server, and
//! read again whenever the Mac mounts or unmounts something.

use std::collections::HashSet;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

struct Watch {
    /// `(dev, ino)` of each directory holding a network mount point.
    dirs: RwLock<HashSet<(i64, u64)>>,
    /// Whether `dirs` has anything in it: a Mac with no network volume, most
    /// of them, pays one load per request and no lock.
    any: AtomicBool,
}

impl Watch {
    fn scan(&self) {
        let dirs: HashSet<(i64, u64)> = holding_dirs(&crate::sys::mounts())
            .iter()
            .filter_map(|dir| std::fs::metadata(dir).ok())
            .map(|meta| (meta.dev() as i64, meta.ino()))
            .collect();
        let any = !dirs.is_empty();
        *self.dirs.write().expect("mount dirs poisoned") = dirs;
        self.any.store(any, Ordering::Release);
    }
}

fn watch() -> &'static Watch {
    static WATCH: OnceLock<Watch> = OnceLock::new();
    WATCH.get_or_init(|| {
        let first = Watch {
            dirs: RwLock::new(HashSet::new()),
            any: AtomicBool::new(false),
        };
        first.scan();
        match crate::sys::filesystem_events() {
            Ok(kq) => {
                let spawned =
                    std::thread::Builder::new()
                        .name("fs-mounts".into())
                        .spawn(move || {
                            while crate::sys::next_filesystem_event(kq.as_raw_fd()).is_ok() {
                                watch().scan();
                            }
                        });
                if let Err(error) = spawned {
                    tracing::warn!(%error, "cannot watch for volumes mounted later");
                }
            }
            Err(errno) => tracing::warn!(errno, "cannot watch for volumes mounted later"),
        }
        first
    })
}

/// Whether the directory `(dev, ino)` holds a network volume's mount point.
pub fn holds_network_mount(dev: i64, ino: u64) -> bool {
    let watch = watch();
    #[cfg(test)]
    if tests::PRETENDED
        .lock()
        .expect("pretended poisoned")
        .contains(&(dev, ino))
    {
        return true;
    }
    watch.any.load(Ordering::Acquire)
        && watch
            .dirs
            .read()
            .expect("mount dirs poisoned")
            .contains(&(dev, ino))
}

/// The directories, of `mounts` (each place and whether it is local), that
/// hold a network volume's mount point. One that is itself on a network
/// volume is left out: requests there wait on a server already, and asking
/// where it is on disk would wait on that server here.
fn holding_dirs(mounts: &[(PathBuf, bool)]) -> Vec<PathBuf> {
    let on_local = |dir: &Path| {
        mounts
            .iter()
            .filter(|(on, _)| dir.starts_with(on))
            .max_by_key(|(on, _)| on.components().count())
            .is_none_or(|(_, local)| *local)
    };
    mounts
        .iter()
        .filter(|(_, local)| !local)
        .filter_map(|(on, _)| on.parent())
        .filter(|dir| on_local(dir))
        .map(Path::to_path_buf)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Directories a test says hold a network mount, beside the real ones.
    pub(crate) static PRETENDED: std::sync::Mutex<Vec<(i64, u64)>> =
        std::sync::Mutex::new(Vec::new());

    #[test]
    fn a_network_volume_marks_the_directory_it_is_mounted_in() {
        let mounts = [
            (PathBuf::from("/"), true),
            (PathBuf::from("/System/Volumes/Data"), true),
            (PathBuf::from("/Volumes/NAS"), false),
            (PathBuf::from("/Users/me/share"), false),
            (PathBuf::from("/Volumes/NAS/nested"), false),
            (PathBuf::from("/Volumes/Backup"), true),
        ];
        let mut dirs = holding_dirs(&mounts);
        dirs.sort();
        assert_eq!(
            dirs,
            [PathBuf::from("/Users/me"), PathBuf::from("/Volumes")],
            "a local volume marks nothing, and nor does one mounted inside a network volume"
        );
    }

    #[test]
    fn the_mount_table_is_read() {
        let mounts = crate::sys::mounts();
        assert!(
            mounts
                .iter()
                .any(|(on, local)| on == Path::new("/") && *local),
            "the root volume is listed, and local: {mounts:?}"
        );
    }
}
