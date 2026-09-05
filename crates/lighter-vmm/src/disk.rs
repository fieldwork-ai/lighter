//! A disk image: a sparse file the framework's block device serves.
//!
//! The device itself is Apple's; what is ours is the file — created at the
//! size the machine was given, never smaller than what is already there,
//! and measured by what APFS has actually allocated to it, which is what a
//! discard punched back out.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

pub struct Disk {
    path: PathBuf,
    file: File,
    len: u64,
}

impl Disk {
    /// Opens `path`, creating it at `len` bytes when it does not exist. An
    /// existing image keeps its size.
    pub fn open_or_create(path: &Path, len: u64) -> io::Result<Disk> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let actual = file.metadata()?.len();
        let len = if actual == 0 {
            file.set_len(len)?;
            len
        } else {
            actual
        };
        Ok(Disk {
            path: path.to_path_buf(),
            file,
            len,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The device's capacity in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// What the filesystem has actually allocated to the image: the sparse
    /// file's blocks, not its length.
    pub fn allocated_bytes(&self) -> io::Result<u64> {
        // SAFETY: fstat into a zeroed struct we own.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(self.file.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((st.st_blocks as u64) * 512)
    }
}
