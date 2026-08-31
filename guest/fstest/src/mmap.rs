//! Memory-mapped access to a shared file.
//!
//! Worth its own module because it is the one thing a FUSE server can get
//! *architecturally* wrong rather than merely incorrectly: a filesystem that
//! answers `open` with `FOPEN_DIRECT_IO` cannot be mapped `MAP_SHARED` at all,
//! and the failure is an `ENODEV` from `mmap` rather than anything about the
//! file. Since half of what runs in a container maps its files — every dynamic
//! loader, sqlite, most language runtimes — a share that cannot be mapped is
//! not a share.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::Path;

const LENGTH: usize = 64 * 1024;

/// A mapping of a host file, read through the page cache.
pub fn shared_mapping(dir: &Path) -> Result<(), String> {
    let path = dir.join("mapped-read");
    let payload: Vec<u8> = (0..LENGTH).map(|i| (i % 251) as u8).collect();
    fs::write(&path, &payload).map_err(|e| e.to_string())?;

    let file = OpenOptions::new()
        .read(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    let map = Mapping::new(&file, LENGTH, libc::PROT_READ)?;
    if map.as_slice() != payload.as_slice() {
        return Err("the mapping does not match the file".into());
    }
    drop(map);
    fs::remove_file(&path).map_err(|e| e.to_string())
}

/// A write through a `MAP_SHARED` mapping, which must reach the file.
pub fn write_through(dir: &Path) -> Result<(), String> {
    let path = dir.join("mapped-write");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        file.write_all(&vec![0u8; LENGTH]).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    let mut map = Mapping::new(&file, LENGTH, libc::PROT_READ | libc::PROT_WRITE)?;
    map.as_mut_slice()[..7].copy_from_slice(b"through");
    map.sync()?;
    drop(map);

    let back = fs::read(&path).map_err(|e| e.to_string())?;
    if &back[..7] != b"through" {
        return Err(format!(
            "the file starts with {:?} after an msync",
            String::from_utf8_lossy(&back[..7])
        ));
    }
    fs::remove_file(&path).map_err(|e| e.to_string())
}

struct Mapping {
    addr: *mut libc::c_void,
    len: usize,
}

impl Mapping {
    fn new(file: &fs::File, len: usize, prot: libc::c_int) -> Result<Mapping, String> {
        // SAFETY: a live descriptor, a length the file is known to have, and a
        // null hint, which asks the kernel to choose the address.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                prot,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            let error = std::io::Error::last_os_error();
            return Err(format!(
                "mmap MAP_SHARED failed: {error} \
                 (a share opened with direct I/O cannot be mapped at all)"
            ));
        }
        Ok(Mapping { addr, len })
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: the mapping is live for the lifetime of `self` and covers
        // exactly `len` bytes.
        unsafe { std::slice::from_raw_parts(self.addr as *const u8, self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` makes the borrow exclusive.
        unsafe { std::slice::from_raw_parts_mut(self.addr as *mut u8, self.len) }
    }

    fn sync(&self) -> Result<(), String> {
        // SAFETY: a live mapping of exactly this length.
        if unsafe { libc::msync(self.addr, self.len, libc::MS_SYNC) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: a live mapping this type owns.
        unsafe { libc::munmap(self.addr, self.len) };
    }
}
