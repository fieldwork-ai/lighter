//! The host file behind a virtio block device.
//!
//! # Why this file is sparse, and why that is the point
//!
//! A disk image here is a plain file that is *logically* as large as the guest's
//! disk but only *physically* as large as what the guest has written. macOS
//! allocates nothing for the untouched parts, and `F_PUNCHHOLE` gives the blocks
//! back when the guest discards them.
//!
//! That pairing is what makes a lighter VM's disk dynamic. Fixed-size disk
//! images are the reason a container runtime's storage only ever grows: the
//! guest deletes an image layer, the guest filesystem marks the blocks free, and
//! the host file stays exactly as big as it was. Wiring the guest's discard
//! through to a hole punch is the whole fix, and it is why
//! `VIRTIO_BLK_F_DISCARD` is not optional for us.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;

/// Bytes per sector, as virtio-blk defines it. Fixed by the specification
/// regardless of the guest's filesystem block size.
pub const SECTOR_SIZE: u64 = 512;

/// `fcntl` command to deallocate a byte range, leaving a hole.
const F_PUNCHHOLE: libc::c_int = 99;

/// Mirrors `fpunchhole_t` from `<sys/fcntl.h>`.
#[repr(C)]
struct FPunchhole {
    fp_flags: libc::c_uint,
    reserved: libc::c_uint,
    fp_offset: libc::off_t,
    fp_length: libc::off_t,
}

/// A disk image backed by a sparse host file.
#[derive(Debug)]
pub struct Disk {
    file: File,
    /// Logical size in bytes; the guest sees this as the disk's capacity.
    len: u64,
    read_only: bool,
}

impl Disk {
    /// Opens an existing image, or creates a sparse one of `len` bytes.
    ///
    /// Creation writes nothing: `set_len` extends the file logically, and macOS
    /// allocates blocks only where data is written. A freshly created 64 GiB
    /// disk occupies zero bytes.
    pub fn open_or_create(path: &Path, len: u64, read_only: bool) -> io::Result<Disk> {
        let file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .create(!read_only)
            .open(path)?;

        let actual_len = file.metadata()?.len();
        let len = if actual_len == 0 && !read_only {
            file.set_len(len)?;
            len
        } else {
            actual_len
        };

        Ok(Disk {
            file,
            len,
            read_only,
        })
    }

    /// Wraps an already-open file, for tests.
    pub fn from_file(file: File, read_only: bool) -> io::Result<Disk> {
        let len = file.metadata()?.len();
        Ok(Disk {
            file,
            len,
            read_only,
        })
    }

    /// Capacity in 512-byte sectors, which is what the guest's config space
    /// reports.
    pub const fn capacity_sectors(&self) -> u64 {
        self.len / SECTOR_SIZE
    }

    pub const fn len(&self) -> u64 {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Bytes actually allocated on the host.
    ///
    /// This is what makes the dynamic disk testable rather than merely claimed:
    /// the gate writes a file in the guest, deletes it, runs `fstrim`, and
    /// asserts that this number went back down.
    pub fn allocated_bytes(&self) -> io::Result<u64> {
        // SAFETY: fstat writes a stat struct through a valid descriptor.
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let rc = unsafe { libc::fstat(self.file.as_raw_fd(), stat.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstat succeeded, so the struct is initialized.
        let stat = unsafe { stat.assume_init() };
        // st_blocks is always in 512-byte units, independent of st_blksize.
        Ok(stat.st_blocks as u64 * 512)
    }

    fn check_range(&self, offset: u64, len: u64) -> io::Result<()> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "disk offset overflow"))?;
        if end > self.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "access at {offset}+{len} is past the end of a {}-byte disk",
                    self.len
                ),
            ));
        }
        Ok(())
    }

    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.check_range(offset, buf.len() as u64)?;
        self.file.read_exact_at(buf, offset)
    }

    pub fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "disk is read-only",
            ));
        }
        self.check_range(offset, buf.len() as u64)?;
        self.file.write_all_at(buf, offset)
    }

    /// Flushes written data to stable storage.
    ///
    /// `File::sync_data` maps to `fsync`, which on macOS returns once the data
    /// reaches the drive's cache rather than the platter. That is the same
    /// guarantee every other Mac container runtime provides, and going further
    /// with `F_FULLFSYNC` would cost roughly an order of magnitude on every
    /// flush the guest issues.
    pub fn flush(&self) -> io::Result<()> {
        if self.read_only {
            return Ok(());
        }
        self.file.sync_data()
    }

    /// Deallocates a byte range, returning its blocks to the host.
    ///
    /// This is the guest's `discard` — `fstrim`, or a filesystem mounted with
    /// `-o discard` — arriving as a hole punch. Reads of the punched range
    /// afterwards return zeroes, which is what the guest already believes.
    pub fn punch_hole(&self, offset: u64, len: u64) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "disk is read-only",
            ));
        }
        self.check_range(offset, len)?;
        if len == 0 {
            return Ok(());
        }

        let arg = FPunchhole {
            fp_flags: 0,
            reserved: 0,
            fp_offset: offset as libc::off_t,
            fp_length: len as libc::off_t,
        };
        // SAFETY: a valid descriptor and a correctly-shaped fpunchhole_t that
        // outlives the call.
        let rc = unsafe { libc::fcntl(self.file.as_raw_fd(), F_PUNCHHOLE, &arg) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Writes zeroes over a range.
    ///
    /// When `may_unmap` is set the guest has told us it does not care whether
    /// the blocks stay allocated, so punching is both correct and free; if the
    /// punch fails we still owe it zeroes, and fall back to writing them.
    pub fn write_zeroes(&self, offset: u64, len: u64, may_unmap: bool) -> io::Result<()> {
        if may_unmap && self.punch_hole(offset, len).is_ok() {
            return Ok(());
        }
        self.check_range(offset, len)?;

        const CHUNK: usize = 1 << 20;
        let zeros = vec![0u8; CHUNK.min(len as usize)];
        let mut written = 0u64;
        while written < len {
            let n = ((len - written) as usize).min(zeros.len());
            self.write_at(offset + written, &zeros[..n])?;
            written += n as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_disk(len: u64) -> (Disk, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "lighter-disk-{}-{:?}.img",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let disk = Disk::open_or_create(&path, len, false).unwrap();
        (disk, path)
    }

    #[test]
    fn a_new_image_is_logically_large_and_physically_empty() {
        let (disk, path) = temp_disk(64 << 20);
        assert_eq!(disk.len(), 64 << 20);
        assert_eq!(disk.capacity_sectors(), (64 << 20) / 512);
        // The headline property: a 64 MiB disk that has cost nothing yet.
        assert_eq!(
            disk.allocated_bytes().unwrap(),
            0,
            "a fresh sparse image must allocate no blocks"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn round_trips_data() {
        let (disk, path) = temp_disk(1 << 20);
        let payload: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        disk.write_at(8192, &payload).unwrap();
        let mut read_back = vec![0u8; payload.len()];
        disk.read_at(8192, &mut read_back).unwrap();
        assert_eq!(read_back, payload);
        let _ = std::fs::remove_file(path);
    }

    /// The property the whole dynamic-disk story rests on: written blocks are
    /// allocated, and discarding them gives the space back.
    ///
    /// # Do not shrink this fixture
    ///
    /// APFS fully allocates a sparse file on its first write when the file is
    /// small — measured on macOS 26: a 16 MiB image written at offset 0
    /// allocates all 16 MiB, while every size from 32 MiB up allocates only
    /// what was written. This test was originally 16 MiB and failed for that
    /// reason alone, which reads exactly like a broken hole punch.
    ///
    /// Real disk images are gigabytes, so the heuristic never applies in
    /// practice; the fixture just has to stay above it to measure the thing it
    /// claims to measure.
    #[test]
    fn punching_a_hole_returns_space_to_the_host() {
        let (disk, path) = temp_disk(256 << 20);
        let payload = vec![0xabu8; 4 << 20];
        disk.write_at(0, &payload).unwrap();
        disk.flush().unwrap();

        let after_write = disk.allocated_bytes().unwrap();
        assert!(
            after_write >= 4 << 20,
            "expected at least 4 MiB allocated, got {after_write}"
        );

        disk.punch_hole(0, 4 << 20).unwrap();
        let after_punch = disk.allocated_bytes().unwrap();
        assert!(
            after_punch < after_write / 2,
            "punching freed nothing: {after_write} -> {after_punch}"
        );

        // The guest believes discarded blocks read as zero, so they must.
        let mut buf = vec![0xffu8; 4096];
        disk.read_at(0, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0), "punched range must read zero");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn accesses_past_the_end_are_refused() {
        let (disk, path) = temp_disk(1 << 20);
        let mut buf = [0u8; 512];
        assert!(disk.read_at((1 << 20) - 256, &mut buf).is_err());
        assert!(disk.write_at(1 << 20, &[0u8; 512]).is_err());
        // An overflowing offset must be rejected, not wrap into a valid range.
        assert!(disk.read_at(u64::MAX - 16, &mut buf).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_only_disks_refuse_every_mutation() {
        let (disk, path) = temp_disk(1 << 20);
        drop(disk);
        let disk = Disk::open_or_create(&path, 0, true).unwrap();
        assert!(disk.write_at(0, &[1u8; 512]).is_err());
        assert!(disk.punch_hole(0, 512).is_err());
        assert!(disk.flush().is_ok(), "flushing a read-only disk is a no-op");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn write_zeroes_without_unmap_still_zeroes() {
        let (disk, path) = temp_disk(1 << 20);
        disk.write_at(0, &[0xffu8; 8192]).unwrap();
        disk.write_zeroes(0, 8192, false).unwrap();
        let mut buf = vec![0xaau8; 8192];
        disk.read_at(0, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0));
        let _ = std::fs::remove_file(path);
    }
}
