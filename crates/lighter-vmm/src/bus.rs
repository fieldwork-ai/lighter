//! MMIO device dispatch.
//!
//! A guest touching an address with no RAM behind it takes a stage-2 data
//! abort, which is how every device access reaches us. The syndrome register
//! describes the access completely — size, direction, and which guest register
//! it targets — so we can service it without ever decoding the instruction.
//!
//! That "without ever decoding" matters: the fallback, when the syndrome is not
//! valid, is to fetch and interpret the faulting instruction ourselves. We
//! refuse to, and explain why at [`MmioFault::decode`].

use std::sync::{Arc, Mutex};

use lighter_hv::Exception;

use crate::layout::Window;

/// A device that answers memory-mapped accesses.
///
/// Offsets are relative to the device's own window, so a device never knows
/// where it was placed — which is what lets the memory map move without
/// touching a device model.
pub trait MmioDevice: Send {
    /// Services a read. `data` is 1, 2, 4 or 8 bytes and must be fully written.
    fn read(&mut self, offset: u64, data: &mut [u8]);

    /// Services a write.
    fn write(&mut self, offset: u64, data: &[u8]);

    /// A short name for diagnostics.
    fn name(&self) -> &'static str;

    /// An interrupt status word the bus may serve without this device's
    /// lock, if the device keeps one. See [`LockfreeInterrupt`].
    fn lockfree_interrupt(&self) -> Option<LockfreeInterrupt> {
        None
    }
}

/// An interrupt status register served from an atomic, outside the device
/// lock.
///
/// Every device lives behind one mutex, and the two registers a guest
/// touches on every interrupt — read the status, write the acknowledgement
/// — took it too. A completion interrupt lands on whichever vCPU the GIC
/// chooses, and its acknowledgement then queued behind the vCPU submitting
/// the next request, which holds the lock for the whole of the request's
/// service. A stream of 4 KiB writes spent as long in that wait as in the
/// guest. Reads of `status_offset` and writes of `ack_offset` (write-one-
/// to-clear) go to `status` directly.
#[derive(Clone)]
pub struct LockfreeInterrupt {
    pub status: Arc<std::sync::atomic::AtomicU32>,
    pub status_offset: u64,
    pub ack_offset: u64,
    pub line: Arc<InterruptLine>,
}

/// A level-triggered line that follows a status word.
///
/// The line is high exactly while the status is non-zero. Raising (a device
/// thread sets a bit) and acknowledging (the guest clears bits) each change
/// the status and then re-derive the line under one lock, so the two can
/// interleave any way they like and the line still ends up matching the
/// status. Driving the line as two independent two-step sequences does not:
/// a raise that set the bit and lifted the line, followed by an
/// acknowledgement of an *earlier* interrupt that dropped the line, left the
/// status set with the line low — a completion the guest was never told
/// about, and a data disk wedged one lap behind its driver.
pub struct InterruptLine {
    status: Arc<std::sync::atomic::AtomicU32>,
    irq: Arc<dyn crate::irq::IrqLine>,
    level: std::sync::Mutex<bool>,
}

impl InterruptLine {
    pub fn new(
        status: Arc<std::sync::atomic::AtomicU32>,
        irq: Arc<dyn crate::irq::IrqLine>,
    ) -> InterruptLine {
        InterruptLine {
            status,
            irq,
            level: std::sync::Mutex::new(false),
        }
    }

    /// Sets `bits` in the status and lifts the line.
    pub fn raise(&self, bits: u32) {
        self.status
            .fetch_or(bits, std::sync::atomic::Ordering::AcqRel);
        self.sync();
    }

    /// Clears `bits` from the status and drops the line if nothing is left.
    pub fn acknowledge(&self, bits: u32) {
        self.status
            .fetch_and(!bits, std::sync::atomic::Ordering::AcqRel);
        self.sync();
    }

    /// Makes the line match the status.
    ///
    /// The status is re-read under the lock, so the last of two racing
    /// callers decides with the latest value, whichever changed it.
    fn sync(&self) {
        let mut level = self.level.lock().expect("interrupt line poisoned");
        let want = self.status.load(std::sync::atomic::Ordering::Acquire) != 0;
        if *level != want {
            self.irq.set_level(want);
            *level = want;
        }
    }
}

/// A decoded MMIO access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmioFault {
    /// Guest-physical address touched.
    pub address: u64,
    /// Access width in bytes: 1, 2, 4 or 8.
    pub size: usize,
    /// True for a store, false for a load.
    pub is_write: bool,
    /// Index of the guest register carrying (or receiving) the value.
    pub reg: u8,
    /// Whether the destination register is 64-bit.
    pub reg_is_64bit: bool,
    /// Whether a loaded value must be sign-extended into the register.
    pub sign_extend: bool,
}

/// Why a data abort could not be turned into a device access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FaultError {
    #[error(
        "data abort at {address:#x} carried no instruction syndrome (ISV=0); \
         lighter does not emulate instructions, so the guest performed an \
         access no device model can service (typically a misaligned or \
         multi-register access to a device window)"
    )]
    NoSyndrome { address: u64 },
    #[error(
        "data abort at {address:#x} is a translation fault, not a device access (DFSC={dfsc:#x})"
    )]
    NotDeviceAccess { address: u64, dfsc: u8 },
}

impl MmioFault {
    /// Decodes a stage-2 data abort into a device access.
    ///
    /// # On not decoding instructions
    ///
    /// When `ISV` is clear the architecture gives us no description of the
    /// access, and a VMM's only recourse is to read the faulting instruction
    /// out of guest memory and decode it — which means implementing a chunk of
    /// the A64 load/store encoding, in the fault path, correctly, forever.
    ///
    /// We do not. Every access a driver makes to a device window is a plain
    /// single-register load or store, which always sets `ISV`. A fault without
    /// it means the guest did something no device could serve anyway, and a
    /// loud error beats a subtly wrong emulation.
    pub fn decode(exception: &Exception) -> Result<MmioFault, FaultError> {
        let iss = exception.iss();
        let address = exception.physical_address;

        // DFSC bits 5:0. Values 0b0001xx are translation faults at some level,
        // which is exactly what an access to an unbacked address produces.
        let dfsc = (iss & 0x3f) as u8;
        let is_translation_fault = (dfsc & 0b111100) == 0b000100;
        if !is_translation_fault {
            return Err(FaultError::NotDeviceAccess { address, dfsc });
        }

        let isv = (iss >> 24) & 1 == 1;
        if !isv {
            return Err(FaultError::NoSyndrome { address });
        }

        let sas = (iss >> 22) & 0b11;
        Ok(MmioFault {
            address,
            size: 1usize << sas,
            is_write: (iss >> 6) & 1 == 1,
            reg: ((iss >> 16) & 0b11111) as u8,
            reg_is_64bit: (iss >> 15) & 1 == 1,
            sign_extend: (iss >> 21) & 1 == 1,
        })
    }

    /// Widens a loaded value to the 64 bits that will be written back into the
    /// destination register, honouring the sign-extension the load asked for.
    pub fn extend_loaded_value(&self, raw: u64) -> u64 {
        let bits = (self.size * 8) as u32;
        if bits >= 64 {
            return raw;
        }
        let masked = raw & ((1u64 << bits) - 1);
        if !self.sign_extend {
            return masked;
        }
        // Sign-extend from the access width, then narrow to 32 bits if the
        // destination register is a W register.
        let shift = 64 - bits;
        let signed = ((masked << shift) as i64 >> shift) as u64;
        if self.reg_is_64bit {
            signed
        } else {
            signed & 0xffff_ffff
        }
    }
}

/// The set of MMIO windows and the devices behind them.
#[derive(Clone, Default)]
pub struct MmioBus {
    entries: Vec<Entry>,
}

#[derive(Clone)]
struct Entry {
    window: Window,
    device: Arc<Mutex<dyn MmioDevice>>,
    interrupt: Option<LockfreeInterrupt>,
}

#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("device window {base:#x}..{end:#x} overlaps an already-registered device")]
    Overlap { base: u64, end: u64 },
}

impl MmioBus {
    pub fn new() -> MmioBus {
        MmioBus::default()
    }

    /// Places a device at a window.
    pub fn register(
        &mut self,
        window: Window,
        device: Arc<Mutex<dyn MmioDevice>>,
    ) -> Result<(), BusError> {
        if self
            .entries
            .iter()
            .any(|e| window.base < e.window.end() && e.window.base < window.end())
        {
            return Err(BusError::Overlap {
                base: window.base,
                end: window.end(),
            });
        }
        let interrupt = device
            .lock()
            .expect("device mutex poisoned")
            .lockfree_interrupt();
        self.entries.push(Entry {
            window,
            device,
            interrupt,
        });
        self.entries.sort_by_key(|e| e.window.base);
        Ok(())
    }

    fn find(&self, address: u64) -> Option<&Entry> {
        // Linear over a handful of devices beats a map: the list is short, it
        // is cache-resident, and this is the hot path for every device access.
        self.entries.iter().find(|e| e.window.contains(address))
    }

    /// Reads from whichever device owns `address`.
    ///
    /// An access to an unclaimed address reads as zero rather than failing:
    /// Linux probes speculatively (a virtio-mmio slot's magic number, a PCI
    /// config space that is not there) and expects a well-behaved bus to
    /// answer, not to fault.
    pub fn read(&self, address: u64, data: &mut [u8]) -> bool {
        match self.find(address) {
            Some(entry) => {
                let offset = address - entry.window.base;
                if let Some(interrupt) = &entry.interrupt
                    && offset == interrupt.status_offset
                    && data.len() == 4
                {
                    let status = interrupt.status.load(std::sync::atomic::Ordering::Acquire);
                    data.copy_from_slice(&status.to_le_bytes());
                    return true;
                }
                entry
                    .device
                    .lock()
                    .expect("device mutex poisoned")
                    .read(offset, data);
                true
            }
            None => {
                data.fill(0);
                false
            }
        }
    }

    /// Writes to whichever device owns `address`. Writes to unclaimed
    /// addresses are dropped, for the same reason reads return zero.
    pub fn write(&self, address: u64, data: &[u8]) -> bool {
        match self.find(address) {
            Some(entry) => {
                let offset = address - entry.window.base;
                if let Some(interrupt) = &entry.interrupt
                    && offset == interrupt.ack_offset
                    && data.len() == 4
                {
                    let value = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                    interrupt.line.acknowledge(value);
                    return true;
                }
                entry
                    .device
                    .lock()
                    .expect("device mutex poisoned")
                    .write(offset, data);
                true
            }
            None => false,
        }
    }
}

impl std::fmt::Debug for MmioBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut list = f.debug_list();
        for entry in &self.entries {
            let name = entry
                .device
                .lock()
                .map(|d| d.name())
                .unwrap_or("<poisoned>");
            list.entry(&format_args!(
                "{name} @ {:#x}..{:#x}",
                entry.window.base,
                entry.window.end()
            ));
        }
        list.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exception(iss: u32, pa: u64) -> Exception {
        Exception {
            syndrome: ((Exception::EC_DATA_ABORT_LOWER_EL as u64) << 26) | u64::from(iss),
            virtual_address: 0,
            physical_address: pa,
        }
    }

    /// ISS for a well-formed single-register access, which is all a driver
    /// ever issues to a device window.
    fn iss(size_log2: u32, is_write: bool, reg: u32, sf: bool, sse: bool) -> u32 {
        let dfsc = 0b000101; // translation fault, level 1
        (1 << 24)
            | (size_log2 << 22)
            | ((sse as u32) << 21)
            | (reg << 16)
            | ((sf as u32) << 15)
            | ((is_write as u32) << 6)
            | dfsc
    }

    #[test]
    fn decodes_access_width_and_direction() {
        for (log2, size) in [(0, 1), (1, 2), (2, 4), (3, 8)] {
            let f = MmioFault::decode(&exception(iss(log2, false, 5, true, false), 0xc00_0000))
                .unwrap();
            assert_eq!(f.size, size);
            assert!(!f.is_write);
            assert_eq!(f.reg, 5);
        }
        let w = MmioFault::decode(&exception(iss(2, true, 9, true, false), 0xc00_0000)).unwrap();
        assert!(w.is_write);
        assert_eq!(w.reg, 9);
    }

    #[test]
    fn refuses_to_guess_without_a_syndrome() {
        let no_isv = iss(2, false, 0, true, false) & !(1 << 24);
        assert!(matches!(
            MmioFault::decode(&exception(no_isv, 0xc00_0000)),
            Err(FaultError::NoSyndrome { .. })
        ));
    }

    #[test]
    fn rejects_aborts_that_are_not_device_accesses() {
        // A permission fault (DFSC 0b001101) is not an unbacked-address access.
        let perm = (iss(2, false, 0, true, false) & !0x3f) | 0b001101;
        assert!(matches!(
            MmioFault::decode(&exception(perm, 0x4000_0000)),
            Err(FaultError::NotDeviceAccess { .. })
        ));
    }

    #[test]
    fn sign_extension_follows_the_access_width() {
        let unsigned = MmioFault::decode(&exception(iss(1, false, 0, true, false), 0)).unwrap();
        assert_eq!(unsigned.extend_loaded_value(0xffff), 0xffff);

        let signed = MmioFault::decode(&exception(iss(1, false, 0, true, true), 0)).unwrap();
        assert_eq!(signed.extend_loaded_value(0xffff), u64::MAX);

        // Into a W register the same load stops at 32 bits.
        let signed_w = MmioFault::decode(&exception(iss(1, false, 0, false, true), 0)).unwrap();
        assert_eq!(signed_w.extend_loaded_value(0xffff), 0xffff_ffff);
    }

    struct Recorder {
        last: Option<(u64, Vec<u8>)>,
        answer: u8,
    }

    impl MmioDevice for Recorder {
        fn read(&mut self, _offset: u64, data: &mut [u8]) {
            data.fill(self.answer);
        }
        fn write(&mut self, offset: u64, data: &[u8]) {
            self.last = Some((offset, data.to_vec()));
        }
        fn name(&self) -> &'static str {
            "recorder"
        }
    }

    #[test]
    fn dispatches_by_window_and_offsets_relative_to_it() {
        let mut bus = MmioBus::new();
        let dev = Arc::new(Mutex::new(Recorder {
            last: None,
            answer: 0xab,
        }));
        bus.register(
            Window {
                base: 0xc00_0000,
                size: 0x1000,
            },
            dev.clone(),
        )
        .unwrap();

        assert!(bus.write(0xc00_0010, &[1, 2, 3, 4]));
        assert_eq!(dev.lock().unwrap().last, Some((0x10, vec![1, 2, 3, 4])));

        let mut buf = [0u8; 4];
        assert!(bus.read(0xc00_0000, &mut buf));
        assert_eq!(buf, [0xab; 4]);
    }

    #[test]
    fn unclaimed_addresses_read_zero_rather_than_faulting() {
        let bus = MmioBus::new();
        let mut buf = [0xffu8; 4];
        assert!(!bus.read(0xdead_0000, &mut buf));
        assert_eq!(buf, [0; 4], "a probe must see zero, not stale bytes");
    }

    #[test]
    fn rejects_overlapping_windows() {
        let mut bus = MmioBus::new();
        let dev = || {
            Arc::new(Mutex::new(Recorder {
                last: None,
                answer: 0,
            })) as Arc<Mutex<dyn MmioDevice>>
        };
        let w = Window {
            base: 0xc00_0000,
            size: 0x1000,
        };
        bus.register(w, dev()).unwrap();
        assert!(matches!(
            bus.register(
                Window {
                    base: 0xc00_0800,
                    size: 0x1000
                },
                dev()
            ),
            Err(BusError::Overlap { .. })
        ));
    }
}
