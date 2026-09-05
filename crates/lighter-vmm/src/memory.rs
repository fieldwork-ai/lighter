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
use std::sync::Arc;

use lighter_hv::{MemoryPerms, Vm};

/// A failure addressing guest memory.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("this GuestMemory has no VM, so regions cannot be mapped into a guest")]
    Detached,
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
                // `LIGHTER_MEM_SHARED=1`: a shared mapping, to measure whether
                // the host's double charge on host-written guest pages is the
                // private mapping's.
                if std::env::var("LIGHTER_MEM_SHARED")
                    .map(|v| v == "1")
                    .unwrap_or(false)
                {
                    libc::MAP_ANON | libc::MAP_SHARED | libc::MAP_NORESERVE
                } else {
                    libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_NORESERVE
                },
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
///
/// Holds the `Vm` because tearing a region down has a mandatory order: remove
/// it from the guest's address space *first*, then release the host pages. The
/// reverse leaves the guest with stage-2 entries pointing at memory the host has
/// reused, which is a use-after-free the guest performs on our behalf.
#[derive(Debug)]
pub struct GuestMemory {
    vm: Option<Arc<Vm>>,
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
    /// Memory belonging to a VM. Regions added here are unmapped on drop.
    pub fn new(vm: Arc<Vm>) -> GuestMemory {
        GuestMemory {
            vm: Some(vm),
            regions: Vec::new(),
        }
    }

    /// An address space with no VM behind it, for tests of code that only
    /// reads and writes bytes. `add_region` on one of these fails.
    pub fn detached() -> GuestMemory {
        GuestMemory {
            vm: None,
            regions: Vec::new(),
        }
    }

    /// Allocates `len` bytes of host RAM and maps it into the guest at `gpa`.
    pub fn add_region(&mut self, gpa: u64, len: usize) -> Result<()> {
        let vm = self.vm.clone().ok_or(MemoryError::Detached)?;
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
        // push, and `Drop for GuestMemory` removes the guest mapping before
        // that Region — and therefore the host pages — goes away.
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

    /// The host address of `len` bytes of guest memory, for a kernel call
    /// that fills guest pages directly (`preadv` into a reply chain).
    ///
    /// A raw pointer, deliberately: no reference to guest memory may exist,
    /// because the guest may be writing the same bytes. The span is only
    /// valid while `self` lives, and the caller must not read or write it
    /// through anything but a syscall or a volatile copy.
    pub fn host_span(&self, gpa: u64, len: usize) -> Result<*mut u8> {
        Ok(self.region_for(gpa, len)?.host_addr(gpa))
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

    /// Returns a span of guest memory to macOS.
    ///
    /// This is the mechanism behind "the VM gives memory back": the mapping
    /// stays, so the guest can touch these addresses again at any time, but the
    /// physical pages are released and the next access faults in a fresh zero
    /// page. Both users — the balloon and free page reporting — are telling us
    /// the guest does not care what is there, which is exactly the contract
    /// `MADV_FREE_REUSABLE` wants.
    ///
    /// # Guest pages are smaller than host pages
    ///
    /// The guest reports 4 KiB pages; Apple silicon hosts use 16 KiB ones. A
    /// release that is not aligned to a *host* page frees nothing at all, so the
    /// aligned interior is what gets released and the ragged edges are dropped.
    /// This is why the balloon coalesces runs before calling: one 4 KiB page is
    /// never releasable, but four contiguous ones are.
    ///
    /// Returns the number of bytes actually released.
    pub fn release(&self, gpa: u64, len: u64) -> Result<u64> {
        let page = host_page_size();
        let start = gpa.div_ceil(page) * page;
        let end = (gpa + len) / page * page;
        if end <= start {
            // The span does not cover a whole host page; nothing to release.
            return Ok(0);
        }

        let span = (end - start) as usize;
        let region = self.region_for(start, span)?;
        let addr = region.host_addr(start);

        // The guest is what dirtied these pages, through the second-stage
        // translation the hypervisor set up — and while that translation
        // exists, macOS will not take them back. `madvise` returns success and
        // the process's footprint does not move, which is the most unhelpful
        // combination of outcomes available.
        //
        // So the mapping is withdrawn for the length of the call. That is safe
        // precisely here and nowhere else: the guest reports free pages
        // synchronously, having first taken them off its own free lists, and
        // it waits for this buffer to come back before it releases them again.
        // There is no moment in between when it could fault on one.
        //
        // SAFETY: no vCPU can be executing code that touches this range, for
        // the reason above.
        let unmapped = match &self.vm {
            // SAFETY: no vCPU can be executing code that touches this range,
            // for the reason above.
            Some(vm) => unsafe { vm.unmap(start, span) }.is_ok(),
            None => false,
        };

        // SAFETY: `addr` is inside a live mapping of at least `span` bytes,
        // checked above. MADV_FREE_REUSABLE does not unmap: the address stays
        // valid and reads fault in zeroes, which is what the guest expects of
        // memory it told us it was not using.
        // A page macOS compressed while the guest was using it, then freed
        // by the guest and released here, kept its compressed copy for the
        // life of the process: `madvise` walks resident pages, and neither
        // MADV_FREE_REUSABLE nor MADV_FREE touched the compressor's — 620
        // MiB of a 977 MiB footprint after an install suite, none of it in
        // the guest, and 864 on the next run. So a span of two megabytes or
        // more has its mapping replaced instead (a fresh anonymous mapping
        // has no pages of any kind): the same suite then read 135 MiB
        // compressed and 517 at the floor, with 254 entries in the address
        // map. Safe exactly here: the stage-2 mapping is down for the length
        // of the call, so no vCPU can see the old pages go.
        // `LIGHTER_RELEASE=reusable|free` are the other two, for the A/B.
        let rc = match release_mode() {
            ReleaseMode::Remap if unmapped && span >= REMAP_MIN_SPAN => {
                // SAFETY: a fixed anonymous mapping over a span this process
                // owns, with the second-stage translation withdrawn above.
                let fresh = unsafe {
                    libc::mmap(
                        addr.cast(),
                        span,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE,
                        -1,
                        0,
                    )
                };
                if fresh == libc::MAP_FAILED { -1 } else { 0 }
            }
            ReleaseMode::Free => {
                // SAFETY: as below, the same live mapping.
                unsafe { libc::madvise(addr.cast(), span, libc::MADV_FREE) };
                unsafe { libc::madvise(addr.cast(), span, MADV_FREE_REUSABLE) }
            }
            _ => unsafe { libc::madvise(addr.cast(), span, MADV_FREE_REUSABLE) },
        };
        let released = if rc == 0 {
            span as u64
        } else {
            let err = io::Error::last_os_error();
            // Not fatal: failing to release memory costs footprint, not
            // correctness, and killing the guest over it would be worse. Loud,
            // though — a share of memory that never comes back is the single
            // thing people notice about running containers in a VM, and a
            // silent `madvise` failure is how it would happen.
            tracing::warn!(%err, gpa, len, span, "could not release guest memory to the host");
            0
        };

        if unmapped {
            // Back before anything can want it. A failure here is not
            // recoverable — the guest would fault on memory it is entitled to
            // — so it is reported rather than swallowed.
            // SAFETY: the same range that was just unmapped, restored to the
            // permissions it had.
            let restored = match &self.vm {
                // SAFETY: the same range that was just unmapped, restored to
                // the permissions it had.
                Some(vm) => unsafe { vm.map(addr.cast(), start, span, MemoryPerms::RWX) },
                None => Ok(()),
            };
            if let Err(err) = restored {
                tracing::error!(%err, gpa = start, span, "could not restore a released mapping");
                return Err(MemoryError::Map(err));
            }
        }
        Ok(released)
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        // Order is the whole point: every region leaves the guest's address
        // space before `self.regions` — and with it the host mappings — is
        // dropped. Skipping this also leaks the guest-physical range, so a
        // later VM in the same process cannot map the same address again.
        if let Some(vm) = &self.vm {
            for region in &self.regions {
                // SAFETY: no vCPU can be running: GuestMemory is held by the
                // Machine, which joins every vCPU thread before dropping it.
                unsafe {
                    let _ = vm.unmap(region.gpa, region.len);
                }
            }
        }
    }
}

/// macOS: pages can be reused by anyone.
const MADV_FREE_REUSABLE: libc::c_int = 7;

/// The host's page size, which on Apple silicon is 16 KiB rather than the 4 KiB
/// the guest uses.
fn host_page_size() -> u64 {
    use std::sync::OnceLock;
    static SIZE: OnceLock<u64> = OnceLock::new();
    *SIZE.get_or_init(|| {
        // SAFETY: sysconf with a valid name has no side effects.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if size > 0 { size as u64 } else { 16384 }
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReleaseMode {
    /// `MADV_FREE_REUSABLE` alone, as shipped through 0.3.0.
    Reusable,
    /// `MADV_FREE` first, for what it may drop, then `MADV_FREE_REUSABLE`.
    Free,
    /// Replace the span's mapping (the default; see `release`).
    Remap,
}

/// Spans below this are released with `madvise` even in remap mode: each
/// replaced span is an entry of its own in the process's address map, and
/// hurried reporting hands over runs down to 128 KiB.
const REMAP_MIN_SPAN: usize = 2 << 20;

/// `LIGHTER_RELEASE=reusable|free|remap`, read once.
fn release_mode() -> ReleaseMode {
    static MODE: std::sync::OnceLock<ReleaseMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("LIGHTER_RELEASE").as_deref() {
        Ok("free") => ReleaseMode::Free,
        Ok("reusable") => ReleaseMode::Reusable,
        _ => ReleaseMode::Remap,
    })
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
