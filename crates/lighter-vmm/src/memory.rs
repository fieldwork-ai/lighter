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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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
    #[error("allocating owned guest backing failed with Mach status {0}")]
    OwnedMapping(i32),
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
        let mapping = Self::reserve(len)?;
        #[cfg(target_os = "macos")]
        {
            let _owned = crate::boot_timing::Phase::new("memory_owned_objects");
            replace_owned_pages(mapping.ptr, len)?;
        }
        Ok(mapping)
    }

    fn reserve(len: usize) -> Result<Mmap> {
        let reservation = crate::boot_timing::Phase::new("memory_reservation");
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
        let mapping = Mmap {
            ptr: ptr.cast(),
            len,
        };
        drop(reservation);
        Ok(mapping)
    }
}

// Owner-accounted objects are charged once even when both the host pmap and
// Hypervisor.framework map their pages. A single large owned object cannot
// discard a subrange while its other mappings keep it alive, so each host
// page gets its own object. Removing one then releases the whole object.
// VM_FLAGS_PURGABLE creates NONVOLATILE objects; we never make live RAM
// volatile, reusable, or exempt from the task's memory ledger.
#[cfg(target_os = "macos")]
fn replace_owned_pages(ptr: *mut u8, len: usize) -> Result<()> {
    unsafe extern "C" {
        static mach_task_self_: libc::c_uint;
        fn mach_vm_allocate(task: libc::c_uint, address: *mut u64, size: u64, flags: i32) -> i32;
    }
    const VM_FLAGS_OVERWRITE: i32 = 0x4000;
    const VM_FLAGS_PURGABLE: i32 = 0x2;
    let page = host_page_size() as usize;
    // Match mach_task_self() in mach/mach_init.h: libSystem caches the task
    // port. Calling the similarly named function takes a kernel trap for
    // every page instead. libSystem initializes this before our threads run.
    // SAFETY: reading libSystem's initialized current-process task port.
    let task = unsafe { mach_task_self_ };
    for offset in (0..len).step_by(page) {
        let mut address = ptr as u64 + offset as u64;
        // SAFETY: replace only pages in the caller's reserved mapping. The
        // guest mapping is absent during allocation and reclamation, and
        // callers retain exclusive ownership of these pages until remapped.
        let result = unsafe {
            mach_vm_allocate(
                task,
                &mut address,
                page as u64,
                VM_FLAGS_OVERWRITE | VM_FLAGS_PURGABLE,
            )
        };
        if result != 0 {
            return Err(MemoryError::OwnedMapping(result));
        }
    }
    Ok(())
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
    deferred: Option<Deferred>,
    demand: Option<Demand>,
}

#[derive(Debug)]
struct Deferred {
    ready: AtomicUsize,
    preparation: Mutex<()>,
}

// Preparation batches do not change allocation ownership: every backing
// object remains one host page. Striped locks bound metadata for large VMs.
const DEMAND_CHUNK: usize = 256 << 10;
#[cfg(test)]
std::thread_local! {
    static DEMAND_FAILURE: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
    static RELEASE_SPANS: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}
#[derive(Debug)]
struct Demand {
    ready: Vec<AtomicBool>,
    locks: Vec<Mutex<()>>,
    complete: AtomicBool,
}

impl Region {
    #[inline]
    fn contains(&self, gpa: u64, len: usize) -> bool {
        let Some(end) = gpa.checked_add(len as u64) else {
            return false;
        };
        let ready = self
            .deferred
            .as_ref()
            .map_or(self.len, |d| d.ready.load(Ordering::Acquire));
        gpa >= self.gpa && end <= self.gpa + ready as u64
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

    /// Real anonymous memory without a hypervisor mapping, for device tests.
    #[cfg(test)]
    pub(crate) fn test_region(gpa: u64, len: usize) -> Self {
        let backing = Mmap::anonymous(len).unwrap();
        Self {
            vm: None,
            regions: vec![Region {
                gpa,
                len,
                host: backing.ptr,
                _backing: backing,
                deferred: None,
                demand: None,
            }],
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
        {
            let _mapping = crate::boot_timing::Phase::new("memory_hypervisor_map");
            unsafe { vm.map(host.cast(), gpa, len, MemoryPerms::RWX)? };
        }

        self.regions.push(Region {
            gpa,
            len,
            host,
            _backing: backing,
            deferred: None,
            demand: None,
        });
        self.regions.sort_by_key(|r| r.gpa);
        Ok(())
    }

    /// Reserve an inaccessible hotplug range. Only `prepare_next` exposes it.
    pub(crate) fn reserve_region(&mut self, gpa: u64, len: usize) -> Result<()> {
        if self.vm.is_none() {
            return Err(MemoryError::Detached);
        }
        let end = gpa
            .checked_add(len as u64)
            .ok_or(MemoryError::OutOfBounds { gpa, len })?;
        if self
            .regions
            .iter()
            .any(|r| gpa < r.gpa + r.len as u64 && r.gpa < end)
        {
            return Err(MemoryError::Overlap { gpa });
        }
        let backing = Mmap::reserve(len)?;
        self.regions.push(Region {
            gpa,
            len,
            host: backing.ptr,
            _backing: backing,
            deferred: Some(Deferred {
                ready: AtomicUsize::new(0),
                preparation: Mutex::new(()),
            }),
            demand: None,
        });
        self.regions.sort_by_key(|r| r.gpa);
        Ok(())
    }

    /// Advertise addressable RAM whose backing is created before first access.
    /// Retains virtio-mem's logical plug/unplug protocol.
    pub(crate) fn reserve_demand_region(&mut self, gpa: u64, len: usize) -> Result<()> {
        self.reserve_region(gpa, len)?;
        let region = self.regions.iter_mut().find(|r| r.gpa == gpa).unwrap();
        region.deferred = None;
        region.demand = Some(Demand {
            ready: (0..len.div_ceil(DEMAND_CHUNK))
                .map(|_| AtomicBool::new(false))
                .collect(),
            locks: (0..64).map(|_| Mutex::new(())).collect(),
            complete: AtomicBool::new(false),
        });
        Ok(())
    }

    fn prepare_demand_chunk(&self, region: &Region, index: usize) -> Result<bool> {
        let demand = region.demand.as_ref().expect("demand region");
        if demand.ready[index].load(Ordering::Acquire) {
            return Ok(false);
        }
        let _lock = demand.locks[index % demand.locks.len()]
            .lock()
            .expect("demand backing poisoned");
        if demand.ready[index].load(Ordering::Acquire) {
            return Ok(false);
        }
        let offset = index * DEMAND_CHUNK;
        let len = DEMAND_CHUNK.min(region.len - offset);
        let gpa = region.gpa + offset as u64;
        let address = region.host_addr(gpa);
        // No CPU or device can access this chunk before publication. A later
        // attempt never replaces a published chunk, even after page reporting.
        let prepare = || -> Result<()> {
            #[cfg(test)]
            let injected_failure = DEMAND_FAILURE.with(|fault| fault.replace(0));
            #[cfg(test)]
            if injected_failure == 1 {
                return Err(MemoryError::OwnedMapping(-1));
            }
            #[cfg(target_os = "macos")]
            replace_owned_pages(address, len)?;
            #[cfg(test)]
            if injected_failure == 2 {
                return Err(MemoryError::Detached);
            }
            let vm = self.vm.as_ref().ok_or(MemoryError::Detached)?;
            // SAFETY: independent owned backing is complete and stable until drop.
            unsafe {
                vm.map(address.cast(), gpa, len, MemoryPerms::RWX)?;
            }
            Ok(())
        };
        if let Err(error) = prepare() {
            // A secondary CPU or a device thread must not die alone and leave
            // the rest of the guest running with an unserviceable RAM promise.
            // Match background preparation's failure policy for the entire VM.
            tracing::error!(%error, gpa, len, "cannot prepare demand memory");
            std::process::abort();
        }
        demand.ready[index].store(true, Ordering::Release);
        Ok(true)
    }

    /// Finish demand regions without waiting for guest accesses. Publication
    /// happens after the final preparation lock is released. Ready chunks are
    /// never replaced, including chunks reclaimed while this worker runs.
    pub(crate) fn prepare_remaining(&self, shutdown: &AtomicBool) -> Result<()> {
        for region in &self.regions {
            let Some(demand) = &region.demand else {
                continue;
            };
            if demand.complete.load(Ordering::Acquire) {
                continue;
            }
            let started = std::time::Instant::now();
            let mut worker_bytes = 0;
            for index in 0..demand.ready.len() {
                if shutdown.load(Ordering::Acquire) {
                    return Ok(());
                }
                if self.prepare_demand_chunk(region, index)? {
                    worker_bytes += DEMAND_CHUNK.min(region.len - index * DEMAND_CHUNK);
                }
            }
            // A ready flag can be observed just before its preparer unlocks.
            // Drain those critical sections before publishing the fast path.
            for lock in &demand.locks {
                drop(lock.lock().expect("demand backing poisoned"));
            }
            demand.complete.store(true, Ordering::Release);
            tracing::info!(
                gpa = region.gpa,
                bytes = region.len,
                worker_bytes,
                access_bytes = region.len - worker_bytes,
                elapsed_us = started.elapsed().as_micros() as u64,
                "RAM preparation complete"
            );
        }
        Ok(())
    }

    /// Only stage-2 translation faults in demand RAM are retried. Device MMIO,
    /// permission faults and other aborts retain their existing handling.
    pub(crate) fn resolve_demand_fault(&self, exception: lighter_hv::Exception) -> Result<bool> {
        if !matches!(
            exception.class(),
            lighter_hv::Exception::EC_DATA_ABORT_LOWER_EL
                | lighter_hv::Exception::EC_INSN_ABORT_LOWER_EL
        ) || !(4..=7).contains(&(exception.iss() & 0x3f))
        {
            return Ok(false);
        }
        let gpa = exception.physical_address;
        let Some(region) = self
            .regions
            .iter()
            .find(|r| r.demand.is_some() && r.contains(gpa, 1))
        else {
            return Ok(false);
        };
        self.prepare_demand_chunk(region, (gpa - region.gpa) as usize / DEMAND_CHUNK)?;
        Ok(true)
    }

    /// Prepare one prefix extension, publishing it only after accounting and
    /// the guest mapping are established. Existing pages are never replaced.
    pub(crate) fn prepare_next(&self, gpa: u64, amount: usize) -> Result<usize> {
        let region = self
            .regions
            .iter()
            .find(|r| r.gpa == gpa)
            .ok_or(MemoryError::OutOfBounds { gpa, len: amount })?;
        let deferred = region
            .deferred
            .as_ref()
            .ok_or(MemoryError::OutOfBounds { gpa, len: amount })?;
        let _exclusive = deferred
            .preparation
            .lock()
            .expect("memory preparation poisoned");
        let start = deferred.ready.load(Ordering::Acquire);
        let end = start.saturating_add(amount).min(region.len);
        if end == start {
            return Ok(end);
        }
        let page = host_page_size() as usize;
        if !start.is_multiple_of(page) || !end.is_multiple_of(page) {
            return Err(MemoryError::OutOfBounds { gpa, len: amount });
        }
        let address = region.host_addr(gpa + start as u64);
        #[cfg(target_os = "macos")]
        replace_owned_pages(address, end - start)?;
        let vm = self.vm.as_ref().ok_or(MemoryError::Detached)?;
        // SAFETY: this prefix extension has never been exposed to the guest or
        // device models. Owned backing is complete and remains alive in Region.
        unsafe {
            vm.map(
                address.cast(),
                gpa + start as u64,
                end - start,
                MemoryPerms::RWX,
            )?;
        }
        deferred.ready.store(end, Ordering::Release);
        Ok(end)
    }

    /// Total bytes of guest RAM.
    pub fn len(&self) -> usize {
        self.regions.iter().map(|r| r.len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    #[inline]
    fn region_for_release(&self, gpa: u64, len: usize) -> Result<&Region> {
        self.regions
            .iter()
            .find(|r| r.contains(gpa, len))
            .ok_or(MemoryError::OutOfBounds { gpa, len })
    }

    #[inline]
    fn region_for(&self, gpa: u64, len: usize) -> Result<&Region> {
        let region = self.region_for_release(gpa, len)?;
        if region
            .demand
            .as_ref()
            .is_some_and(|d| !d.complete.load(Ordering::Acquire))
            && len != 0
        {
            let first = (gpa - region.gpa) as usize / DEMAND_CHUNK;
            let last = ((gpa - region.gpa) as usize + len - 1) / DEMAND_CHUNK;
            for index in first..=last {
                self.prepare_demand_chunk(region, index)?;
            }
        }
        Ok(region)
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
    /// Volatile where the address allows it, because the guest can be
    /// writing the same location and a plain read would let the optimizer
    /// hoist or duplicate it; volatile bytes where it does not, since the
    /// guest is not obliged to align what it puts in a buffer (virtio's
    /// rings are aligned by the specification, a device header is wherever
    /// the driver's allocation fell), and a typed volatile read of an
    /// unaligned address is undefined behaviour.
    pub fn read_u32(&self, gpa: u64) -> Result<u32> {
        let region = self.region_for(gpa, 4)?;
        // SAFETY: bounds checked above.
        let value = unsafe { read_prim(region.host_addr(gpa).cast::<u32>()) };
        Ok(u32::from_le(value))
    }

    pub fn write_u32(&self, gpa: u64, value: u32) -> Result<()> {
        let region = self.region_for(gpa, 4)?;
        // SAFETY: bounds checked above.
        unsafe { write_prim(region.host_addr(gpa).cast::<u32>(), value.to_le()) };
        Ok(())
    }

    pub fn read_u16(&self, gpa: u64) -> Result<u16> {
        let region = self.region_for(gpa, 2)?;
        // SAFETY: bounds checked above.
        let value = unsafe { read_prim(region.host_addr(gpa).cast::<u16>()) };
        Ok(u16::from_le(value))
    }

    pub fn write_u16(&self, gpa: u64, value: u16) -> Result<()> {
        let region = self.region_for(gpa, 2)?;
        // SAFETY: bounds checked above.
        unsafe { write_prim(region.host_addr(gpa).cast::<u16>(), value.to_le()) };
        Ok(())
    }

    pub fn read_u64(&self, gpa: u64) -> Result<u64> {
        let region = self.region_for(gpa, 8)?;
        // SAFETY: bounds checked above.
        let value = unsafe { read_prim(region.host_addr(gpa).cast::<u64>()) };
        Ok(u64::from_le(value))
    }

    pub fn write_u64(&self, gpa: u64, value: u64) -> Result<()> {
        let region = self.region_for(gpa, 8)?;
        // SAFETY: bounds checked above.
        unsafe { write_prim(region.host_addr(gpa).cast::<u64>(), value.to_le()) };
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

    /// Discards the aligned interior of a guest-owned free span. The guest
    /// must keep it unused until the device returns its descriptor.
    ///
    /// Fresh backing objects are essential, including for ordinary page
    /// reporting and single host pages. MADV_FREE_REUSABLE removes pages from
    /// phys_footprint, but guest reuse never issues MADV_FREE_REUSE: live pages
    /// then remain uncounted. Remapping has no reusable accounting state, and
    /// also releases compressed pages. The next access is a charged zero page.
    pub fn release(&self, gpa: u64, len: u64) -> Result<u64> {
        let len_usize = usize::try_from(len).map_err(|_| MemoryError::OutOfBounds {
            gpa,
            len: usize::MAX,
        })?;
        // Validate the original range before alignment; wrapping must never
        // turn an invalid descriptor into a valid release of unrelated RAM.
        let region = self.region_for_release(gpa, len_usize)?;
        let end = gpa.checked_add(len).ok_or(MemoryError::OutOfBounds {
            gpa,
            len: len_usize,
        })?;
        let page = host_page_size();
        let start = gpa.checked_add(page - 1).ok_or(MemoryError::OutOfBounds {
            gpa,
            len: len_usize,
        })? / page
            * page;
        let end = end / page * page;
        if end <= start {
            return Ok(0);
        }
        if let Some(demand) = &region.demand {
            if demand.complete.load(Ordering::Acquire) {
                return self.release_backed(region, start, end);
            }
            let mut at = start;
            let mut released = 0;
            while at < end {
                let index = (at - region.gpa) as usize / DEMAND_CHUNK;
                let next = end.min(region.gpa + ((index + 1) * DEMAND_CHUNK) as u64);
                let _lock = demand.locks[index % demand.locks.len()]
                    .lock()
                    .expect("demand backing poisoned");
                // An untouched chunk already has no physical backing. Reporting
                // it must not allocate it or race a first access into replacing
                // newly published live pages.
                if demand.ready[index].load(Ordering::Acquire) {
                    released += self.release_backed(region, at, next)?;
                }
                at = next;
            }
            return Ok(released);
        }
        self.release_backed(region, start, end)
    }

    fn release_backed(&self, region: &Region, start: u64, end: u64) -> Result<u64> {
        let span = (end - start) as usize;
        #[cfg(test)]
        RELEASE_SPANS.with(|spans| spans.borrow_mut().push(span));
        let addr = region.host_addr(start);

        if let Some(vm) = &self.vm {
            // SAFETY: the guest has handed ownership of this free span to
            // the device until completion. Do not discard if unmapping fails.
            unsafe { vm.unmap(start, span)? };
        }
        #[cfg(target_os = "macos")]
        if let Err(error) = replace_owned_pages(addr, span) {
            // Overwrite may have removed backing before a failed allocation.
            // No device may acknowledge or reuse a partially replaced range.
            tracing::error!(%error, start, span, "cannot replace released memory");
            std::process::abort();
        }
        #[cfg(not(target_os = "macos"))]
        {
            // SAFETY: an aligned, exclusively owned subrange with no guest
            // mapping. Non-Mach hosts retain ordinary anonymous backing.
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
            if fresh == libc::MAP_FAILED {
                tracing::error!(error = %io::Error::last_os_error(), start, span, "cannot replace released memory");
                std::process::abort();
            }
        }
        if let Some(vm) = &self.vm {
            // SAFETY: the fresh host mapping has the same address/length and
            // outlives the restored guest mapping just as the original did.
            if let Err(error) = unsafe { vm.map(addr.cast(), start, span, MemoryPerms::RWX) } {
                // Device callers may treat an ordinary release error as no
                // reclaim. They must never return a now-unmapped page to Linux.
                tracing::error!(%error, start, span, "cannot restore released guest memory");
                std::process::abort();
            }
        }
        Ok(span as u64)
    }

    /// Balloon and virtio-mem use the same accounting-safe release as reporting.
    pub fn release_thoroughly(&self, gpa: u64, len: u64) -> Result<u64> {
        self.release(gpa, len)
    }

    /// Coalesce a reporting request before changing mappings. All its buffers
    /// remain owned by the host until the entire request completes. Validate
    /// first, and never merge across separately allocated backing regions.
    pub(crate) fn release_reported(&self, spans: &mut [(u64, u64)]) -> Result<u64> {
        for &(gpa, len) in spans.iter() {
            self.region_for_release(
                gpa,
                usize::try_from(len).map_err(|_| MemoryError::OutOfBounds {
                    gpa,
                    len: usize::MAX,
                })?,
            )?;
        }
        spans.sort_unstable();
        let mut total = 0;
        let mut run: Option<(u64, u64)> = None;
        for &(start, len) in spans.iter() {
            let end = start.checked_add(len).ok_or(MemoryError::OutOfBounds {
                gpa: start,
                len: len as usize,
            })?;
            match run {
                Some((first, last))
                    if start <= last
                        && self
                            .region_for_release(first, (last.max(end) - first) as usize)
                            .is_ok() =>
                {
                    run = Some((first, last.max(end)));
                }
                previous => {
                    if let Some((first, last)) = previous {
                        total += self.release(first, last - first)?;
                    }
                    run = Some((start, end));
                }
            }
        }
        if let Some((first, last)) = run {
            total += self.release(first, last - first)?;
        }
        Ok(total)
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
                if let Some(demand) = &region.demand {
                    // All users have joined. Unmap only published chunks;
                    // unprepared holes have never had stage-2 mappings.
                    for (index, ready) in demand.ready.iter().enumerate() {
                        if ready.load(Ordering::Acquire) {
                            let offset = index * DEMAND_CHUNK;
                            let len = DEMAND_CHUNK.min(region.len - offset);
                            // SAFETY: no vCPU or device can still access memory.
                            unsafe {
                                let _ = vm.unmap(region.gpa + offset as u64, len);
                            }
                        }
                    }
                    continue;
                }
                // SAFETY: no vCPU can be running: GuestMemory is held by the
                // Machine, which joins every vCPU thread before dropping it.
                unsafe {
                    let ready = region
                        .deferred
                        .as_ref()
                        .map_or(region.len, |d| d.ready.load(Ordering::Acquire));
                    if ready != 0 {
                        let _ = vm.unmap(region.gpa, ready);
                    }
                }
            }
        }
    }
}

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

/// A primitive read from guest memory at whatever alignment the guest chose:
/// one volatile access when aligned, volatile bytes otherwise.
///
/// # Safety
/// `ptr` must be valid for a read of `T` inside a live mapping. `T` must
/// have no padding and accept every bit pattern (only integer callers here).
unsafe fn read_prim<T: Copy>(ptr: *const T) -> T {
    if (ptr as usize).is_multiple_of(std::mem::align_of::<T>()) {
        // SAFETY: aligned, and valid by the caller's contract.
        unsafe { ptr::read_volatile(ptr) }
    } else {
        let mut value = std::mem::MaybeUninit::<T>::uninit();
        for i in 0..std::mem::size_of::<T>() {
            // SAFETY: byte pointers have alignment one; every byte of this
            // integer is copied once, without a reference into guest memory.
            unsafe {
                value
                    .as_mut_ptr()
                    .cast::<u8>()
                    .add(i)
                    .write(ptr.cast::<u8>().add(i).read_volatile())
            };
        }
        // SAFETY: all bytes initialized and all bit patterns valid for T.
        unsafe { value.assume_init() }
    }
}

/// The write to match `read_prim`.
///
/// # Safety
/// `ptr` must be valid for a write of `T` inside a live mapping; `T` has no padding.
unsafe fn write_prim<T: Copy>(ptr: *mut T, value: T) {
    if (ptr as usize).is_multiple_of(std::mem::align_of::<T>()) {
        // SAFETY: aligned, and valid by the caller's contract.
        unsafe { ptr::write_volatile(ptr, value) }
    } else {
        for i in 0..std::mem::size_of::<T>() {
            // SAFETY: initialized bytes of a padding-free integer, and a
            // valid destination with byte alignment, per the caller.
            unsafe {
                ptr.cast::<u8>()
                    .add(i)
                    .write_volatile((&value as *const T).cast::<u8>().add(i).read())
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_work_at_every_alignment_and_stay_in_bounds() {
        let base = 0x4000_0000;
        let mem = GuestMemory::test_region(base, 0x4000);
        for offset in 0..8 {
            mem.write_u64(base + offset, 0x0123_4567_89ab_cdef).unwrap();
            assert_eq!(mem.read_u64(base + offset).unwrap(), 0x0123_4567_89ab_cdef);
            mem.write_u32(base + offset, 0x89ab_cdef).unwrap();
            assert_eq!(mem.read_u32(base + offset).unwrap(), 0x89ab_cdef);
            mem.write_u16(base + offset, 0xcdef).unwrap();
            assert_eq!(mem.read_u16(base + offset).unwrap(), 0xcdef);
            let mut bytes = [0; 2];
            mem.read(base + offset, &mut bytes).unwrap();
            assert_eq!(bytes, [0xef, 0xcd]);
        }
        assert!(mem.read_u64(base + 0x4000 - 7).is_err());
        assert!(mem.write_u32(base + 0x4000 - 3, 0).is_err());
        assert!(mem.read_u16(u64::MAX).is_err());
    }

    #[test]
    fn a_single_reported_host_page_is_zeroed_and_edges_are_preserved() {
        let base = 0x4000_0000;
        let page = host_page_size() as usize;
        let mem = GuestMemory::test_region(base, 3 * page);
        mem.write(base, &vec![0x5a; 3 * page]).unwrap();
        assert_eq!(
            mem.release(base + 1, (3 * page - 2) as u64).unwrap(),
            page as u64
        );
        let mut bytes = vec![0; 3 * page];
        mem.read(base, &mut bytes).unwrap();
        assert!(bytes[..page].iter().all(|b| *b == 0x5a));
        assert!(bytes[page..2 * page].iter().all(|b| *b == 0));
        assert!(bytes[2 * page..].iter().all(|b| *b == 0x5a));
        assert!(mem.release(u64::MAX - 3, 8).is_err());
        assert!(mem.release(base, u64::MAX).is_err());
        assert_eq!(mem.release(base + 1, (page - 1) as u64).unwrap(), 0);
    }

    #[test]
    fn reporting_merges_adjacent_fragments_but_not_gaps() {
        let base = 0x4000_0000;
        let page = host_page_size();
        let mem = GuestMemory::test_region(base, (3 * page) as usize);
        mem.write(base, &vec![0x5a; (3 * page) as usize]).unwrap();
        let mut spans = [
            (base + page / 2, page / 2),
            (base + 2 * page, page),
            (base, page / 2),
        ];
        assert_eq!(mem.release_reported(&mut spans).unwrap(), 2 * page);
        assert_eq!(mem.read_u64(base).unwrap(), 0);
        assert_eq!(mem.read_u64(base + page).unwrap(), 0x5a5a5a5a5a5a5a5a);
        assert_eq!(mem.read_u64(base + 2 * page).unwrap(), 0);
        // Validate the whole request before releasing its first valid buffer.
        mem.write_u64(base, 123).unwrap();
        assert!(
            mem.release_reported(&mut [(base, page), (u64::MAX, 2)])
                .is_err()
        );
        assert_eq!(mem.read_u64(base).unwrap(), 123);
    }

    /// A tiny guest writes pages, reports alternating single host pages, then
    /// reuses them. This checks real Hypervisor.framework accounting without
    /// booting Linux, running a workload, or producing benchmark results.
    #[test]
    #[ignore = "requires a signed test binary and Hypervisor.framework"]
    fn reported_pages_are_recharged_after_guest_reuse() {
        guest_accounting_and_reuse(0);
    }

    #[test]
    #[ignore = "requires a signed test binary and Hypervisor.framework"]
    fn deferred_pages_are_recharged_after_guest_reuse() {
        guest_accounting_and_reuse(1);
    }

    #[test]
    #[ignore = "requires a signed test binary and Hypervisor.framework"]
    fn deferred_backing_is_inaccessible_until_prepared_and_never_overwritten() {
        let vm = Arc::new(Vm::create().unwrap());
        let mut mem = GuestMemory::new(vm);
        let base = 0x4000_0000;
        let page = host_page_size() as usize;
        mem.reserve_region(base, 4 * page).unwrap();
        assert!(mem.read_u64(base).is_err());
        assert!(mem.host_span(base, 1).is_err());
        assert!(mem.write_u64(base, 1).is_err());
        assert!(mem.release(base, page as u64).is_err());
        assert!(mem.prepare_next(base, page + 1).is_err());
        assert!(mem.read_u64(base).is_err());
        assert_eq!(mem.prepare_next(base, page).unwrap(), page);
        mem.write_u64(base, 0x1234).unwrap();
        assert!(mem.read_u64(base + page as u64).is_err());
        assert!(mem.reserve_region(base + page as u64, page).is_err());
        std::thread::scope(|scope| {
            scope.spawn(|| mem.prepare_next(base, page).unwrap());
            scope.spawn(|| mem.prepare_next(base, page).unwrap());
        });
        assert_eq!(mem.read_u64(base).unwrap(), 0x1234);
        assert_eq!(mem.read_u64(base + 2 * page as u64).unwrap(), 0);
        assert!(mem.read_u64(base + 3 * page as u64).is_err());
        assert_eq!(mem.prepare_next(base, page).unwrap(), 4 * page);
        assert_eq!(mem.prepare_next(base, page).unwrap(), 4 * page);
        assert_eq!(mem.read_u64(base).unwrap(), 0x1234);
        assert_eq!(mem.release(base, page as u64).unwrap(), page as u64);
        assert_eq!(mem.read_u64(base).unwrap(), 0);
    }

    #[test]
    #[ignore = "requires a signed test binary and Hypervisor.framework"]
    fn demand_pages_fault_in_and_are_recharged_after_guest_reuse() {
        guest_accounting_and_reuse(2);
    }

    #[test]
    #[ignore = "requires a signed test binary and Hypervisor.framework"]
    fn hybrid_pages_are_reclaimed_and_recharged_after_guest_reuse() {
        guest_accounting_and_reuse(3);
    }

    #[test]
    #[ignore = "requires a signed test binary and Hypervisor.framework"]
    fn hybrid_preparation_preserves_live_data_and_restores_whole_range_release() {
        let vm = Arc::new(Vm::create().unwrap());
        let mut mem = GuestMemory::new(vm);
        let base = 0x4000_0000;
        let size = 128 * DEMAND_CHUNK;
        mem.reserve_demand_region(base, size).unwrap();
        // Exercise multiple regions, including a partial final chunk.
        let second = base + 2 * size as u64;
        mem.reserve_demand_region(second, DEMAND_CHUNK + host_page_size() as usize)
            .unwrap();
        mem.write_u64(base, 0xfeed).unwrap();
        let shutdown = AtomicBool::new(true);
        mem.prepare_remaining(&shutdown).unwrap();
        assert!(
            !mem.regions[0]
                .demand
                .as_ref()
                .unwrap()
                .complete
                .load(Ordering::Acquire)
        );
        shutdown.store(false, Ordering::Release);
        std::thread::scope(|scope| {
            scope.spawn(|| mem.prepare_remaining(&shutdown).unwrap());
            scope.spawn(|| {
                // Disjoint live bytes must survive both preparation and release.
                for _ in 0..100 {
                    for chunk in [0, 67, 127] {
                        let address = base + (chunk * DEMAND_CHUNK) as u64;
                        mem.write_u64(address, 0xfeed).unwrap();
                        assert_eq!(mem.read_u64(address).unwrap(), 0xfeed);
                    }
                }
            });
            scope.spawn(|| {
                // Guest-owned free ranges may be reported while backing advances.
                for _ in 0..10 {
                    mem.release(base + DEMAND_CHUNK as u64, 32 * DEMAND_CHUNK as u64)
                        .unwrap();
                }
            });
        });
        for region in &mem.regions {
            let demand = region.demand.as_ref().unwrap();
            assert!(demand.complete.load(Ordering::Acquire));
            assert!(demand.ready.iter().all(|r| r.load(Ordering::Acquire)));
        }
        for chunk in [0, 67, 127] {
            assert_eq!(
                mem.read_u64(base + (chunk * DEMAND_CHUNK) as u64).unwrap(),
                0xfeed
            );
        }
        RELEASE_SPANS.with(|spans| spans.borrow_mut().clear());
        let free = base + DEMAND_CHUNK as u64;
        let length = 3 * DEMAND_CHUNK;
        mem.write_u64(free, 0xbeef).unwrap();
        assert_eq!(mem.release(free, length as u64).unwrap(), length as u64);
        RELEASE_SPANS.with(|spans| assert_eq!(*spans.borrow(), vec![length]));
        assert_eq!(mem.read_u64(free).unwrap(), 0);
        mem.write_u64(free, 0x1234).unwrap();
        mem.prepare_remaining(&shutdown).unwrap();
        assert_eq!(mem.read_u64(free).unwrap(), 0x1234);
    }

    #[test]
    #[ignore = "requires a signed test binary and Hypervisor.framework"]
    fn demand_sparse_concurrent_devices_and_partial_release() {
        let vm = Arc::new(Vm::create().unwrap());
        let mut mem = GuestMemory::new(vm);
        let base = 0x4000_0000;
        let size = 128 * DEMAND_CHUNK;
        mem.reserve_demand_region(base, size).unwrap();
        let demand = mem.regions[0].demand.as_ref().unwrap();
        assert_eq!(mem.release(base, size as u64).unwrap(), 0);
        assert!(demand.ready.iter().all(|r| !r.load(Ordering::Acquire)));
        assert!(mem.host_span(base + size as u64 - 1, 2).is_err());
        std::thread::scope(|scope| {
            for thread in 0..8u64 {
                let mem = &mem;
                scope.spawn(move || {
                    for chunk in [0, 67, 127] {
                        let address = base + chunk * DEMAND_CHUNK as u64 + thread * 8;
                        for _ in 0..100 {
                            mem.write_u64(address, thread + 1).unwrap();
                            assert_eq!(mem.read_u64(address).unwrap(), thread + 1);
                        }
                    }
                });
            }
        });
        assert_eq!(
            demand
                .ready
                .iter()
                .filter(|r| r.load(Ordering::Acquire))
                .count(),
            3
        );
        // A device span crossing preparation boundaries must be fully backed.
        let boundary = base + 2 * DEMAND_CHUNK as u64;
        mem.write(boundary - 4, &[0x5a; 8]).unwrap();
        let ptr = mem.host_span(boundary - 4, 8).unwrap();
        // SAFETY: the span was prepared, and no guest is accessing this fixture.
        unsafe {
            assert_eq!(ptr.add(7).read_volatile(), 0x5a);
        }
        let page = host_page_size();
        mem.write_u64(base + page, 0xfeed).unwrap();
        assert_eq!(mem.release(base, page).unwrap(), page);
        assert_eq!(mem.read_u64(base).unwrap(), 0);
        assert_eq!(mem.read_u64(base + page).unwrap(), 0xfeed);
        assert_eq!(mem.read_u64(base + 127 * DEMAND_CHUNK as u64).unwrap(), 1);
        let mut spans = [(base + 10 * DEMAND_CHUNK as u64, DEMAND_CHUNK as u64)];
        assert_eq!(mem.release_reported(&mut spans).unwrap(), 0);
        assert!(!demand.ready[10].load(Ordering::Acquire));
    }

    #[test]
    #[ignore = "requires a signed test binary and Hypervisor.framework"]
    fn demand_instruction_fault_is_prepared_without_advancing_pc() {
        use lighter_hv::{Exception, Exit, Gic, GicLayout, Reg};
        let vm = Arc::new(Vm::create().unwrap());
        let _gic = Gic::create(&vm, GicLayout::default()).unwrap();
        let mut mem = GuestMemory::new(vm.clone());
        let base = 0x4000_0000;
        mem.reserve_demand_region(base, DEMAND_CHUNK).unwrap();
        let mut cpu = vm.create_vcpu().unwrap();
        cpu.set_trap_debug_exceptions(true).unwrap();
        cpu.set_reg(Reg::Pc, base).unwrap();
        cpu.set_reg(Reg::Cpsr, lighter_hv::PSTATE_EL1H_DAIF_MASKED)
            .unwrap();
        let Exit::Exception(e) = cpu.run().unwrap() else {
            panic!("expected instruction abort")
        };
        assert_eq!(e.class(), Exception::EC_INSN_ABORT_LOWER_EL);
        assert!(mem.resolve_demand_fault(e).unwrap());
        assert_eq!(cpu.reg(Reg::Pc).unwrap(), base);
        // Install a BRK after handling the otherwise uninitialized fetch.
        mem.write_u32(base, 0xd420_0000).unwrap();
        assert!(
            matches!(cpu.run().unwrap(), Exit::Exception(e) if e.class() == Exception::EC_BRK64)
        );
        let bad = Exception {
            syndrome: (Exception::EC_DATA_ABORT_LOWER_EL as u64) << 26 | 4,
            physical_address: base - 1,
            virtual_address: 0,
        };
        assert!(!mem.resolve_demand_fault(bad).unwrap());
        let permission = Exception {
            syndrome: (Exception::EC_DATA_ABORT_LOWER_EL as u64) << 26 | 15,
            physical_address: base,
            virtual_address: base,
        };
        assert!(!mem.resolve_demand_fault(permission).unwrap());
        drop(cpu);
    }

    #[test]
    #[ignore = "requires a signed test binary and Hypervisor.framework"]
    fn simultaneous_vcpu_faults_preserve_each_writers_data() {
        use lighter_hv::{Exception, Exit, Gic, GicLayout, Reg};
        let vm = Arc::new(Vm::create().unwrap());
        let _gic = Gic::create(&vm, GicLayout::default()).unwrap();
        let mut mem = GuestMemory::new(vm.clone());
        let code_base = 0x4000_0000;
        let data = code_base + 0x10_0000;
        let size = 8 << 20;
        mem.add_region(code_base, host_page_size() as usize)
            .unwrap();
        mem.reserve_demand_region(data, size).unwrap();
        // Four CPUs store distinct words on the same initially absent pages.
        for (i, instruction) in [
            0xf900_0002,
            0x9140_0400,
            0xeb01_001f,
            0x54ff_ffa3,
            0xd420_0000,
        ]
        .into_iter()
        .enumerate()
        {
            mem.write_u32(code_base + i as u64 * 4, instruction)
                .unwrap();
        }
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for writer in 0..4u64 {
                let vm = &vm;
                let mem = &mem;
                let barrier = &barrier;
                scope.spawn(move || {
                    let mut cpu = vm.create_vcpu().unwrap();
                    cpu.set_trap_debug_exceptions(true).unwrap();
                    cpu.set_reg(Reg::Pc, code_base).unwrap();
                    cpu.set_reg(Reg::Cpsr, lighter_hv::PSTATE_EL1H_DAIF_MASKED)
                        .unwrap();
                    cpu.set_reg(Reg::X0, data + writer * 8).unwrap();
                    cpu.set_reg(Reg::X1, data + size as u64 + writer * 8)
                        .unwrap();
                    cpu.set_reg(Reg::X2, writer + 1).unwrap();
                    barrier.wait();
                    loop {
                        match cpu.run().unwrap() {
                            Exit::Exception(e) if mem.resolve_demand_fault(e).unwrap() => continue,
                            Exit::Exception(e) if e.class() == Exception::EC_BRK64 => break,
                            other => panic!("unexpected guest exit: {other:?}"),
                        }
                    }
                });
            }
        });
        for offset in (0..size).step_by(4096) {
            for writer in 0..4u64 {
                assert_eq!(
                    mem.read_u64(data + offset as u64 + writer * 8).unwrap(),
                    writer + 1
                );
            }
        }
    }

    #[test]
    #[ignore = "requires a signed test binary and Hypervisor.framework"]
    fn demand_backing_failure_stops_the_entire_process() {
        use std::os::unix::process::ExitStatusExt;
        const CHILD: &str = "LIGHTER_TEST_DEMAND_FAILURE_CHILD";
        if let Ok(mode) = std::env::var(CHILD) {
            // The deliberate abort must not write a core image of test memory.
            let limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: a valid rlimit for the current disposable test process.
            unsafe {
                assert_eq!(libc::setrlimit(libc::RLIMIT_CORE, &limit), 0);
            }
            let vm = Arc::new(Vm::create().unwrap());
            let mut mem = GuestMemory::new(vm);
            mem.reserve_demand_region(0x4000_0000, DEMAND_CHUNK)
                .unwrap();
            DEMAND_FAILURE.with(|fault| fault.set(mode.parse().unwrap()));
            let _ = mem.write_u64(0x4000_0000, 123);
            panic!("failed demand backing returned control to the caller");
        }
        for mode in ["1", "2"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "memory::tests::demand_backing_failure_stops_the_entire_process",
                    "--ignored",
                    "--test-threads=1",
                ])
                .env(CHILD, mode)
                .output()
                .unwrap();
            assert_eq!(output.status.signal(), Some(libc::SIGABRT), "{output:?}");
        }
    }

    fn guest_accounting_and_reuse(mode: u8) {
        use lighter_hv::{Exception, Exit, Gic, GicLayout, Reg};
        const BASE: u64 = 0x4000_0000;
        const SIZE: usize = 32 << 20;
        let vm = Arc::new(Vm::create().unwrap());
        let _gic = Gic::create(&vm, GicLayout::default()).unwrap();
        let mut mem = GuestMemory::new(vm.clone());
        let page = host_page_size() as usize;
        mem.add_region(BASE, page).unwrap();
        let data = BASE + page as u64;
        if mode >= 2 {
            mem.reserve_demand_region(data, SIZE).unwrap();
        } else if mode == 1 {
            mem.reserve_region(data, SIZE).unwrap();
            for _ in 0..4 {
                mem.prepare_next(data, SIZE / 4).unwrap();
            }
        } else {
            mem.add_region(data, SIZE).unwrap();
        }
        // str x2,[x0]; add x0,x0,#4096; cmp x0,x1; b.lo -12; brk #0
        let code = [
            0xf900_0002u32,
            0x9140_0400,
            0xeb01_001f,
            0x54ff_ffa3,
            0xd420_0000,
        ];
        for (i, instruction) in code.into_iter().enumerate() {
            mem.write_u32(BASE + i as u64 * 4, instruction).unwrap();
        }
        let mut vcpu = vm.create_vcpu().unwrap();
        vcpu.set_trap_debug_exceptions(true).unwrap();
        let mut touch = |value| {
            vcpu.set_reg(Reg::Pc, BASE).unwrap();
            vcpu.set_reg(Reg::Cpsr, lighter_hv::PSTATE_EL1H_DAIF_MASKED)
                .unwrap();
            vcpu.set_reg(Reg::X0, data).unwrap();
            vcpu.set_reg(Reg::X1, data + SIZE as u64).unwrap();
            vcpu.set_reg(Reg::X2, value).unwrap();
            loop {
                match vcpu.run().unwrap() {
                    Exit::Exception(e) if mem.resolve_demand_fault(e).unwrap() => continue,
                    Exit::Exception(e) if e.class() == Exception::EC_BRK64 => break,
                    other => panic!("unexpected guest exit: {other:?}"),
                }
            }
        };
        if mode == 3 {
            std::thread::scope(|scope| {
                scope.spawn(|| mem.prepare_remaining(&AtomicBool::new(false)).unwrap());
                touch(1);
            });
            assert!(
                mem.regions[1]
                    .demand
                    .as_ref()
                    .unwrap()
                    .complete
                    .load(Ordering::Acquire)
            );
        } else {
            touch(1);
        }
        for round in 0..3 {
            let guest_charged = crate::footprint::bytes();
            for offset in (0..SIZE).step_by(page) {
                assert_eq!(mem.read_u64(data + offset as u64).unwrap(), round + 1);
            }
            let charged = crate::footprint::bytes();
            assert!(
                charged.saturating_sub(guest_charged) < 4 << 20,
                "host reads charged the same guest pages twice: {guest_charged} -> {charged}"
            );
            // Gaps prevent coalescing: spans below the old 128 KiB threshold
            // must be accounted correctly too, on every reuse cycle.
            let mut spans: Vec<_> = (0..SIZE)
                .step_by(page * 2)
                .map(|offset| (data + offset as u64, page as u64))
                .collect();
            assert_eq!(mem.release_reported(&mut spans).unwrap(), SIZE as u64 / 2);
            let released = crate::footprint::bytes();
            assert!(
                charged.saturating_sub(released) >= SIZE as u64 / 4,
                "reporting did not release physical pages: {charged} -> {released}"
            );
            for offset in (page..SIZE).step_by(page * 2) {
                assert_eq!(mem.read_u64(data + offset as u64).unwrap(), round + 1);
            }
            // Reading every discarded page here would itself recharge them.
            assert_eq!(mem.read_u64(data).unwrap(), 0);
            touch(round + 2);
            let reused = crate::footprint::bytes();
            assert!(
                reused.saturating_sub(released) >= SIZE as u64 / 4,
                "guest reuse was not charged: {released} -> {reused}"
            );
            assert!(
                reused >= charged.saturating_sub(4 << 20),
                "footprint shrank after reuse: {charged} -> {reused}"
            );
        }
        drop(vcpu);
    }

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
            deferred: None,
            demand: None,
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
