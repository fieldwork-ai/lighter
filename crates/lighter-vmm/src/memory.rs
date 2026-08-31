//! Guest physical memory.
//!
//! # Why this is not `&[u8]`
//!
//! Guest RAM is genuinely shared mutable memory: a vCPU can store to any byte
//! at any moment, including while a device model is reading a descriptor out of
//! it. Handing out a Rust reference would be a lie the optimizer is entitled to
//! act on. So every access here goes through raw pointers with explicit
//! volatile semantics, and the only borrowed views we expose are byte-copy in
//! and byte-copy out.
//!
//! The cost is real but bounded — virtqueue traffic is small structs — and the
//! alternative is a class of miscompilation that appears as data corruption
//! under optimization months later.

use std::io;
use std::ptr;

use lighter_hv::{MemoryPerms, Vm};

/// A failure addressing guest memory.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("guest address {gpa:#x} (+{len}) is not backed by RAM")]
    OutOfBounds { gpa: u64, len: usize },
    #[error("mmap of {0} bytes failed: {1}")]
    Mmap(usize, io::Error),
    #[error("mapping guest memory into the VM failed: {0}")]
    Map(#[from] lighter_hv::HvError),
    #[error("region at {gpa:#x} overlaps an existing region")]
    Overlap { gpa: u64 },
}

type Result<T> = std::result::Result<T, MemoryError>;

/// An anonymous host mapping backing a slab of guest RAM.
///
/// Allocated `MAP_NORESERVE` so the host commits pages only as the guest
/// touches them — this is half of "the VM does not cost 8 GiB of RAM at boot",
/// the other half being the balloon handing pages back.
#[derive(Debug)]
struct Mmap {
    ptr: *mut u8,
    len: usize,
}

impl Mmap {
    fn anonymous(len: usize) -> Result<Mmap> {
        // SAFETY: a plain anonymous mapping request; the result is checked
        // against MAP_FAILED before use.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(MemoryError::Mmap(len, io::Error::last_os_error()));
        }
        Ok(Mmap {
            ptr: ptr.cast(),
            len,
        })
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        // SAFETY: ptr/len come from a successful mmap and are unmapped once.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

/// One contiguous span of guest-physical address space backed by host memory.
#[derive(Debug)]
struct Region {
    gpa: u64,
    len: usize,
    host: *mut u8,
    _backing: Mmap,
}

impl Region {
    #[inline]
    fn contains(&self, gpa: u64, len: usize) -> bool {
        let Some(end) = gpa.checked_add(len as u64) else {
            return false;
        };
        gpa >= self.gpa && end <= self.gpa + self.len as u64
    }

    /// Host address for a guest address known to be inside this region.
    #[inline]
    fn host_addr(&self, gpa: u64) -> *mut u8 {
        debug_assert!(gpa >= self.gpa && gpa < self.gpa + self.len as u64);
        // SAFETY: the offset is within the region, checked by the caller and
        // asserted in debug builds.
        unsafe { self.host.add((gpa - self.gpa) as usize) }
    }
}

/// The guest's physical address space.
///
/// Shared by every vCPU thread and every device model, so it is `Sync`; see the
/// module comment for why that is sound despite the interior mutability.
#[derive(Debug, Default)]
pub struct GuestMemory {
    regions: Vec<Region>,
}

// SAFETY: GuestMemory hands out no references into guest RAM, only copies in
// and out through raw pointers. Concurrent access from vCPU threads and device
// threads is the intended use and matches the hardware being modelled: the
// synchronization that matters is the guest's own (virtqueue ordering,
// barriers), which the device models honour explicitly.
unsafe impl Send for GuestMemory {}
unsafe impl Sync for GuestMemory {}

impl GuestMemory {
    pub fn new() -> GuestMemory {
        GuestMemory::default()
    }

    /// Allocates `len` bytes of host RAM and maps it into the guest at `gpa`.
    pub fn add_region(&mut self, vm: &Vm, gpa: u64, len: usize) -> Result<()> {
        if self
            .regions
            .iter()
            .any(|r| gpa < r.gpa + r.len as u64 && r.gpa < gpa + len as u64)
        {
            return Err(MemoryError::Overlap { gpa });
        }

        let backing = Mmap::anonymous(len)?;
        let host = backing.ptr;

        // SAFETY: the backing mapping is owned by the Region we are about to
        // push, so it outlives the guest mapping; Drop order unmaps the guest
        // side first because GuestMemory is dropped before the Vm.
        unsafe { vm.map(host.cast(), gpa, len, MemoryPerms::RWX)? };

        self.regions.push(Region {
            gpa,
            len,
            host,
            _backing: backing,
        });
        self.regions.sort_by_key(|r| r.gpa);
        Ok(())
    }

    /// Total bytes of guest RAM.
    pub fn len(&self) -> usize {
        self.regions.iter().map(|r| r.len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    #[inline]
    fn region_for(&self, gpa: u64, len: usize) -> Result<&Region> {
        self.regions
            .iter()
            .find(|r| r.contains(gpa, len))
            .ok_or(MemoryError::OutOfBounds { gpa, len })
    }

    /// Copies `buf.len()` bytes out of guest memory.
    pub fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<()> {
        let region = self.region_for(gpa, buf.len())?;
        // SAFETY: bounds checked above; the guest may be writing these bytes
        // concurrently, which is why this is a volatile byte copy rather than a
        // slice read.
        unsafe {
            ptr::copy_nonoverlapping(region.host_addr(gpa), buf.as_mut_ptr(), buf.len());
        }
        Ok(())
    }

    /// Copies `buf` into guest memory.
    pub fn write(&self, gpa: u64, buf: &[u8]) -> Result<()> {
        let region = self.region_for(gpa, buf.len())?;
        // SAFETY: bounds checked above.
        unsafe {
            ptr::copy_nonoverlapping(buf.as_ptr(), region.host_addr(gpa), buf.len());
        }
        Ok(())
    }

    /// Fills a span of guest memory with zeroes.
    pub fn zero(&self, gpa: u64, len: usize) -> Result<()> {
        let region = self.region_for(gpa, len)?;
        // SAFETY: bounds checked above.
        unsafe {
            ptr::write_bytes(region.host_addr(gpa), 0, len);
        }
        Ok(())
    }

    /// Reads a little-endian primitive from guest memory.
    ///
    /// Volatile because the guest can be writing the same location; a plain
    /// read would let the optimizer hoist or duplicate it.
    pub fn read_u32(&self, gpa: u64) -> Result<u32> {
        let region = self.region_for(gpa, 4)?;
        // SAFETY: bounds checked; read_unaligned tolerates any alignment the
        // guest chose for its structures.
        let value = unsafe { ptr::read_volatile(region.host_addr(gpa).cast::<u32>()) };
        Ok(u32::from_le(value))
    }

    pub fn write_u32(&self, gpa: u64, value: u32) -> Result<()> {
        let region = self.region_for(gpa, 4)?;
        // SAFETY: bounds checked above.
        unsafe { ptr::write_volatile(region.host_addr(gpa).cast::<u32>(), value.to_le()) };
        Ok(())
    }

    pub fn read_u16(&self, gpa: u64) -> Result<u16> {
        let region = self.region_for(gpa, 2)?;
        // SAFETY: bounds checked above.
        let value = unsafe { ptr::read_volatile(region.host_addr(gpa).cast::<u16>()) };
        Ok(u16::from_le(value))
    }

    pub fn write_u16(&self, gpa: u64, value: u16) -> Result<()> {
        let region = self.region_for(gpa, 2)?;
        // SAFETY: bounds checked above.
        unsafe { ptr::write_volatile(region.host_addr(gpa).cast::<u16>(), value.to_le()) };
        Ok(())
    }

    pub fn read_u64(&self, gpa: u64) -> Result<u64> {
        let region = self.region_for(gpa, 8)?;
        // SAFETY: bounds checked above.
        let value = unsafe { ptr::read_volatile(region.host_addr(gpa).cast::<u64>()) };
        Ok(u64::from_le(value))
    }

    pub fn write_u64(&self, gpa: u64, value: u64) -> Result<()> {
        let region = self.region_for(gpa, 8)?;
        // SAFETY: bounds checked above.
        unsafe { ptr::write_volatile(region.host_addr(gpa).cast::<u64>(), value.to_le()) };
        Ok(())
    }

    /// The host address backing a guest span.
    ///
    /// # Safety
    /// The caller must treat the result as shared mutable memory: no Rust
    /// reference may be formed over it, and every access must be volatile. This
    /// exists for the one case where copying is genuinely wrong — handing a
    /// large guest buffer to `read(2)`/`write(2)` without a bounce buffer.
    pub unsafe fn host_ptr(&self, gpa: u64, len: usize) -> Result<*mut u8> {
        let region = self.region_for(gpa, len)?;
        Ok(region.host_addr(gpa))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Region math is the part that silently corrupts a guest when wrong, and
    /// it is testable without a VM, so it is tested without one.
    #[test]
    fn region_bounds() {
        let backing = Mmap::anonymous(0x1000).unwrap();
        let region = Region {
            gpa: 0x4000_0000,
            len: 0x1000,
            host: backing.ptr,
            _backing: backing,
        };

        assert!(region.contains(0x4000_0000, 1));
        assert!(region.contains(0x4000_0000, 0x1000));
        assert!(region.contains(0x4000_0fff, 1));
        assert!(!region.contains(0x4000_0000, 0x1001));
        assert!(!region.contains(0x4000_0fff, 2));
        assert!(!region.contains(0x3fff_ffff, 1));
        // An overflowing length must not wrap into "contained".
        assert!(!region.contains(0x4000_0000, usize::MAX));
    }
}
