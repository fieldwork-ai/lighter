//! What a `nodeid` and a `fh` mean on this side of the wire.
//!
//! The guest addresses files by two opaque 64-bit numbers it never
//! interprets: a `nodeid` naming an inode, and an `fh` naming an open handle.
//! This module is the only place either is minted or resolved.
//!
//! # Why an inode is a descriptor and not a path
//!
//! A remembered path is wrong the instant anything renames it, and "anything"
//! includes a `git checkout` on the host moving a directory the guest has open.
//! Holding a descriptor per inode makes the identity survive that: the
//! descriptor follows the file, not the name. It is also what makes
//! unlink-while-open behave — an inode with no remaining links still has a live
//! descriptor, so the guest's open file keeps working exactly as it would on a
//! local filesystem.
//!
//! # Lookup counts
//!
//! FUSE is explicit that the kernel owns the lifetime: every reply that carries
//! a `nodeid` increments a count the guest later decrements with FORGET, and
//! the server may only drop an inode when the count reaches zero. Getting this
//! wrong in the generous direction leaks descriptors until the process hits its
//! limit; getting it wrong in the other direction hands the guest a `nodeid`
//! that resolves to nothing, which surfaces as random `ESTALE` under load.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex};

use crate::sys::Dir;

/// The `nodeid` of a mount's root. Fixed by the protocol.
pub const ROOT_ID: u64 = 1;

/// One file, as long as the guest remembers it.
pub struct Inode {
    /// A metadata-only descriptor. Never read or written through; it exists so
    /// the file can be found again after any amount of renaming.
    pub fd: OwnedFd,
    /// Host identity, which is what makes two paths to one file share a
    /// `nodeid` — as hard links must.
    pub dev: i64,
    pub ino: u64,
    pub is_dir: bool,
    pub is_symlink: bool,
    /// How many times the guest has been told about this inode.
    lookups: Mutex<u64>,
}

impl Inode {
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// An open file or directory, named by an `fh`.
pub enum Handle {
    File(OpenFile),
    Dir(OpenDir),
}

/// An open directory.
pub struct OpenDir {
    /// Which directory this is a stream over.
    ///
    /// A `DIR*` cannot be asked, and READDIRPLUS has to look each entry up
    /// under the right parent — so the answer is recorded when the stream is
    /// created rather than reconstructed later from a path.
    pub nodeid: u64,
    /// The mutex is not incidental: `readdir` advances a cursor inside the
    /// `DIR*`, so two threads sharing one would interleave entries and each
    /// see half the directory.
    pub stream: Mutex<Dir>,
}

/// An open regular file.
pub struct OpenFile {
    pub fd: OwnedFd,
    /// Opened `O_APPEND` on the host.
    ///
    /// Worth a field of its own because it changes which syscall writes it:
    /// POSIX has `pwrite` ignore its offset on an append-mode descriptor, so
    /// the offset FUSE supplies would be silently discarded and two writers
    /// would still be safe — but a *non*-append handle must use `pwrite`, and
    /// telling them apart needs this.
    pub append: bool,
}

impl Handle {
    pub fn file(&self) -> Option<&OpenFile> {
        match self {
            Handle::File(file) => Some(file),
            Handle::Dir(_) => None,
        }
    }

    pub fn raw_fd(&self) -> Option<RawFd> {
        match self {
            Handle::File(file) => Some(file.fd.as_raw_fd()),
            Handle::Dir(_) => None,
        }
    }
}

struct Tables {
    by_id: HashMap<u64, Arc<Inode>>,
    /// Host identity to `nodeid`, so a second path to the same file resolves to
    /// the same inode rather than a second one with its own descriptor.
    by_identity: HashMap<(i64, u64), u64>,
    next_id: u64,
}

/// Every inode and open handle the guest currently holds.
pub struct Registry {
    inodes: Mutex<Tables>,
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    next_handle: Mutex<u64>,
}

impl Registry {
    /// Builds a registry whose root is `root_fd`.
    pub fn new(root_fd: OwnedFd, dev: i64, ino: u64) -> Registry {
        let root = Arc::new(Inode {
            fd: root_fd,
            dev,
            ino,
            is_dir: true,
            is_symlink: false,
            // The root is never forgotten: the kernel does not FORGET nodeid 1,
            // and a count that could reach zero would let it be dropped by a
            // buggy guest and take the whole mount with it.
            lookups: Mutex::new(u64::MAX),
        });
        let mut by_id = HashMap::new();
        by_id.insert(ROOT_ID, root);
        let mut by_identity = HashMap::new();
        by_identity.insert((dev, ino), ROOT_ID);
        Registry {
            inodes: Mutex::new(Tables {
                by_id,
                by_identity,
                next_id: ROOT_ID + 1,
            }),
            handles: Mutex::new(HashMap::new()),
            next_handle: Mutex::new(1),
        }
    }

    pub fn get(&self, id: u64) -> Option<Arc<Inode>> {
        self.inodes.lock().expect("inode table poisoned").by_id.get(&id).cloned()
    }

    /// Records an inode the guest is about to be told about, and counts the
    /// telling.
    ///
    /// `fd` is only installed if this is a new inode; otherwise it is dropped
    /// and the existing descriptor kept, which is what keeps one file to one
    /// descriptor however many names it has.
    pub fn insert(&self, fd: OwnedFd, dev: i64, ino: u64, is_dir: bool, is_symlink: bool) -> u64 {
        let mut tables = self.inodes.lock().expect("inode table poisoned");
        if let Some(&id) = tables.by_identity.get(&(dev, ino)) {
            if let Some(inode) = tables.by_id.get(&id) {
                let mut lookups = inode.lookups.lock().expect("lookup count poisoned");
                *lookups = lookups.saturating_add(1);
            }
            return id;
        }
        let id = tables.next_id;
        tables.next_id += 1;
        tables.by_identity.insert((dev, ino), id);
        tables.by_id.insert(
            id,
            Arc::new(Inode {
                fd,
                dev,
                ino,
                is_dir,
                is_symlink,
                lookups: Mutex::new(1),
            }),
        );
        id
    }

    /// Applies a FORGET.
    ///
    /// The inode is dropped from the table when its count reaches zero, but any
    /// operation still in flight holds an `Arc` and keeps the descriptor alive
    /// until it finishes — which is why the worker pool needs no ordering
    /// guarantee between FORGET and everything else.
    pub fn forget(&self, id: u64, count: u64) {
        if id == ROOT_ID {
            return;
        }
        let mut tables = self.inodes.lock().expect("inode table poisoned");
        let Some(inode) = tables.by_id.get(&id) else {
            return;
        };
        let remaining = {
            let mut lookups = inode.lookups.lock().expect("lookup count poisoned");
            *lookups = lookups.saturating_sub(count);
            *lookups
        };
        if remaining == 0 {
            let identity = (inode.dev, inode.ino);
            tables.by_id.remove(&id);
            // Only unmap the identity if it still points at us: a rename that
            // reused the inode number could have repointed it.
            if tables.by_identity.get(&identity) == Some(&id) {
                tables.by_identity.remove(&identity);
            }
        }
    }

    /// How many inodes are live. Diagnostics only.
    pub fn inode_count(&self) -> usize {
        self.inodes.lock().expect("inode table poisoned").by_id.len()
    }

    pub fn add_handle(&self, handle: Handle) -> u64 {
        let mut next = self.next_handle.lock().expect("handle counter poisoned");
        let id = *next;
        *next += 1;
        drop(next);
        self.handles
            .lock()
            .expect("handle table poisoned")
            .insert(id, Arc::new(handle));
        id
    }

    pub fn handle(&self, id: u64) -> Option<Arc<Handle>> {
        self.handles.lock().expect("handle table poisoned").get(&id).cloned()
    }

    pub fn release_handle(&self, id: u64) {
        self.handles.lock().expect("handle table poisoned").remove(&id);
    }

    pub fn handle_count(&self) -> usize {
        self.handles.lock().expect("handle table poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        let fd = crate::sys::open_root(&std::env::temp_dir()).unwrap();
        Registry::new(fd, 1, 1)
    }

    fn spare_fd() -> OwnedFd {
        crate::sys::open_root(&std::env::temp_dir()).unwrap()
    }

    /// Two names for one file must be one inode, or the guest sees two
    /// different `nodeid`s for something `stat` says is the same file — and
    /// `find -samefile` and every build system's hard-link cache disagree with
    /// the kernel.
    #[test]
    fn one_host_file_is_one_nodeid_however_many_names() {
        let reg = registry();
        let first = reg.insert(spare_fd(), 7, 42, false, false);
        let second = reg.insert(spare_fd(), 7, 42, false, false);
        assert_eq!(first, second);
        assert_eq!(reg.inode_count(), 2, "root plus the one file");
    }

    /// Two lookups mean two forgets. Dropping on the first would hand the guest
    /// a dangling nodeid it is still entitled to use.
    #[test]
    fn an_inode_survives_until_every_lookup_is_forgotten() {
        let reg = registry();
        let id = reg.insert(spare_fd(), 7, 42, false, false);
        reg.insert(spare_fd(), 7, 42, false, false);

        reg.forget(id, 1);
        assert!(reg.get(id).is_some(), "one forget of two must not drop it");
        reg.forget(id, 1);
        assert!(reg.get(id).is_none());
        assert_eq!(reg.inode_count(), 1, "only the root remains");
    }

    /// The kernel may batch forgets, and it may over-count after an abort.
    #[test]
    fn a_batched_or_excessive_forget_does_not_underflow() {
        let reg = registry();
        let id = reg.insert(spare_fd(), 7, 42, false, false);
        reg.forget(id, u64::MAX);
        assert!(reg.get(id).is_none());
        reg.forget(id, 1);
    }

    /// The root has no FORGET in the protocol, and a guest that sent one must
    /// not be able to unmount itself.
    #[test]
    fn the_root_cannot_be_forgotten() {
        let reg = registry();
        reg.forget(ROOT_ID, u64::MAX);
        assert!(reg.get(ROOT_ID).is_some());
    }

    /// A forgotten identity must be re-lookupable, with a fresh nodeid rather
    /// than the old one: the guest may still have the old number in flight.
    #[test]
    fn a_reinserted_file_gets_a_new_nodeid() {
        let reg = registry();
        let first = reg.insert(spare_fd(), 7, 42, false, false);
        reg.forget(first, 1);
        let second = reg.insert(spare_fd(), 7, 42, false, false);
        assert_ne!(first, second);
    }

    #[test]
    fn handles_are_issued_and_released() {
        let reg = registry();
        let fh = reg.add_handle(Handle::File(OpenFile { fd: spare_fd(), append: false }));
        assert!(reg.handle(fh).is_some());
        assert_eq!(reg.handle_count(), 1);
        reg.release_handle(fh);
        assert!(reg.handle(fh).is_none());
        assert_eq!(reg.handle_count(), 0);
    }

    /// Handle numbers are never reused. A stale `fh` from a released file must
    /// fail rather than land on whatever was opened next.
    #[test]
    fn handle_numbers_are_not_recycled() {
        let reg = registry();
        let first = reg.add_handle(Handle::File(OpenFile { fd: spare_fd(), append: false }));
        reg.release_handle(first);
        let second = reg.add_handle(Handle::File(OpenFile { fd: spare_fd(), append: false }));
        assert_ne!(first, second);
    }
}
