//! A PL011 UART.
//!
//! This is the first device the guest touches and the only one that works
//! before any driver is loaded — `earlycon` writes to it directly from the
//! kernel's first few hundred instructions. A VMM without one debugs early boot
//! failures by staring at a hang, so this exists before anything else.
//!
//! Only the registers Linux's `amba-pl011` driver actually reads are
//! implemented, plus the AMBA identification registers, which are not optional:
//! the driver binds by peripheral ID, so getting those wrong produces a guest
//! with a device tree node, no driver, and no console.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;

use crate::bus::MmioDevice;
use crate::irq::IrqLine;

// Register offsets.
const UARTDR: u64 = 0x000;
const UARTRSR: u64 = 0x004;
const UARTFR: u64 = 0x018;
const UARTILPR: u64 = 0x020;
const UARTIBRD: u64 = 0x024;
const UARTFBRD: u64 = 0x028;
const UARTLCR_H: u64 = 0x02c;
const UARTCR: u64 = 0x030;
const UARTIFLS: u64 = 0x034;
const UARTIMSC: u64 = 0x038;
const UARTRIS: u64 = 0x03c;
const UARTMIS: u64 = 0x040;
const UARTICR: u64 = 0x044;
const UARTDMACR: u64 = 0x048;

// Flag register bits.
const FR_RXFE: u32 = 1 << 4; // receive FIFO empty
const FR_TXFF: u32 = 1 << 5; // transmit FIFO full
const FR_RXFF: u32 = 1 << 6; // receive FIFO full
const FR_TXFE: u32 = 1 << 7; // transmit FIFO empty

// Interrupt bits, shared by RIS/MIS/IMSC/ICR.
const INT_RX: u32 = 1 << 4;
const INT_TX: u32 = 1 << 5;

/// AMBA identification. The driver computes
/// `(id3 << 24) | (id2 << 16) | (id1 << 8) | id0` and matches it against
/// `0x00041011` under mask `0x000fffff`; these bytes are what produce that.
const PERIPH_ID: [u8; 4] = [0x11, 0x10, 0x14, 0x00];
const PCELL_ID: [u8; 4] = [0x0d, 0xf0, 0x05, 0xb1];

/// How much unread guest input we are willing to hold.
///
/// Bounded because the producer is a host thread reading a terminal and the
/// consumer is a guest that may never drain it — an unbounded queue here is a
/// slow memory leak driven by whatever is pasted into the console.
const RX_CAPACITY: usize = 4096;

/// The UART's receive side, shared with whatever host thread feeds it.
#[derive(Debug, Default)]
pub struct Rx {
    queue: VecDeque<u8>,
}

impl Rx {
    fn push(&mut self, byte: u8) -> bool {
        if self.queue.len() >= RX_CAPACITY {
            return false;
        }
        self.queue.push_back(byte);
        true
    }
}

/// A PL011 UART whose transmit side goes to an arbitrary host sink.
pub struct Pl011 {
    /// Where guest output goes. Boxed so the console can be stdout, a log file,
    /// or a test buffer without the device knowing which.
    sink: Box<dyn Write + Send>,
    irq: Arc<dyn IrqLine>,
    rx: Rx,

    // Programmed state. We keep these because the driver reads back what it
    // wrote and would otherwise conclude the device is broken; none of it
    // affects behaviour, since there is no real wire to configure.
    control: u32,
    line_control: u32,
    ibrd: u32,
    fbrd: u32,
    ifls: u32,
    dmacr: u32,
    ilpr: u32,

    /// Raw interrupt status.
    ris: u32,
    /// Interrupt mask.
    imsc: u32,
}

impl Pl011 {
    pub fn new(sink: Box<dyn Write + Send>, irq: Arc<dyn IrqLine>) -> Pl011 {
        Pl011 {
            sink,
            irq,
            rx: Rx::default(),
            control: 0x0300, // TXE | RXE, the reset value the driver expects
            line_control: 0,
            ibrd: 0,
            fbrd: 0,
            ifls: 0x12,
            dmacr: 0,
            ilpr: 0,
            ris: 0,
            imsc: 0,
        }
    }

    /// Queues a byte of host input for the guest to read.
    ///
    /// Returns false if the guest is not draining and the buffer is full, in
    /// which case the byte is dropped — the same thing a real UART does when
    /// its FIFO overruns.
    pub fn enqueue_input(&mut self, byte: u8) -> bool {
        let accepted = self.rx.push(byte);
        if accepted {
            self.ris |= INT_RX;
            self.refresh_interrupt();
        }
        accepted
    }

    /// Recomputes the outgoing interrupt from raw status and mask.
    ///
    /// Level-driven rather than pulsed: the PL011 line stays asserted until the
    /// driver clears the condition, and pulsing instead would lose the
    /// interrupt whenever the guest was slow to look.
    fn refresh_interrupt(&self) {
        self.irq.set_level(self.ris & self.imsc != 0);
    }

    fn flag_register(&self) -> u32 {
        let mut fr = FR_TXFE; // transmit always drains instantly
        if self.rx.queue.is_empty() {
            fr |= FR_RXFE;
        }
        if self.rx.queue.len() >= RX_CAPACITY {
            fr |= FR_RXFF;
        }
        // TXFF is never set: a host sink that blocks would stall the vCPU, so
        // we always accept the byte and let the sink deal with it.
        debug_assert_eq!(fr & FR_TXFF, 0);
        fr
    }

    fn read_register(&mut self, offset: u64) -> u32 {
        match offset {
            UARTDR => {
                let byte = self.rx.queue.pop_front().unwrap_or(0);
                if self.rx.queue.is_empty() {
                    self.ris &= !INT_RX;
                    self.refresh_interrupt();
                }
                u32::from(byte)
            }
            UARTRSR => 0,
            UARTFR => self.flag_register(),
            UARTILPR => self.ilpr,
            UARTIBRD => self.ibrd,
            UARTFBRD => self.fbrd,
            UARTLCR_H => self.line_control,
            UARTCR => self.control,
            UARTIFLS => self.ifls,
            UARTIMSC => self.imsc,
            UARTRIS => self.ris,
            UARTMIS => self.ris & self.imsc,
            UARTDMACR => self.dmacr,
            0xfe0..=0xfec => u32::from(PERIPH_ID[((offset - 0xfe0) / 4) as usize]),
            0xff0..=0xffc => u32::from(PCELL_ID[((offset - 0xff0) / 4) as usize]),
            _ => 0,
        }
    }

    fn write_register(&mut self, offset: u64, value: u32) {
        match offset {
            UARTDR => {
                let byte = (value & 0xff) as u8;
                // A console write that fails is not worth killing the guest
                // over, and cannot be reported to it either — the guest already
                // believes the byte went out.
                let _ = self.sink.write_all(&[byte]);
                if byte == b'\n' {
                    let _ = self.sink.flush();
                }
                // The transmit FIFO is always empty, so a TX interrupt is
                // immediately eligible; the driver relies on this to send its
                // next chunk.
                self.ris |= INT_TX;
                self.refresh_interrupt();
            }
            UARTRSR => {}
            UARTILPR => self.ilpr = value,
            UARTIBRD => self.ibrd = value,
            UARTFBRD => self.fbrd = value,
            UARTLCR_H => self.line_control = value,
            UARTCR => self.control = value,
            UARTIFLS => self.ifls = value,
            UARTIMSC => {
                self.imsc = value;
                self.refresh_interrupt();
            }
            UARTICR => {
                // Write-one-to-clear.
                self.ris &= !value;
                self.refresh_interrupt();
            }
            UARTDMACR => self.dmacr = value,
            _ => {}
        }
    }
}

impl MmioDevice for Pl011 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        // Every PL011 register is 32 bits; a driver reading a narrower slice
        // gets the low bytes, which is what the hardware does.
        let value = self.read_register(offset & !0x3);
        let bytes = value.to_le_bytes();
        let shift = (offset & 0x3) as usize;
        for (i, out) in data.iter_mut().enumerate() {
            *out = bytes.get(shift + i).copied().unwrap_or(0);
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut buf = [0u8; 4];
        for (i, b) in data.iter().take(4).enumerate() {
            buf[i] = *b;
        }
        self.write_register(offset & !0x3, u32::from_le_bytes(buf));
    }

    fn name(&self) -> &'static str {
        "pl011"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::NullIrq;
    use std::sync::Mutex;

    /// A sink that records what the guest wrote.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn uart() -> (Pl011, Captured) {
        let captured = Captured::default();
        (
            Pl011::new(Box::new(captured.clone()), Arc::new(NullIrq)),
            captured,
        )
    }

    #[test]
    fn guest_writes_reach_the_sink() {
        let (mut uart, out) = uart();
        for byte in b"boot\n" {
            uart.write(UARTDR, &[*byte, 0, 0, 0]);
        }
        assert_eq!(&*out.0.lock().unwrap(), b"boot\n");
    }

    /// The driver binds by peripheral ID. If this is wrong the console is
    /// simply absent, with a correct-looking device tree — the exact failure
    /// this test exists to prevent.
    #[test]
    fn identifies_as_a_pl011_to_the_amba_bus() {
        let (mut uart, _) = uart();
        let mut id = 0u32;
        for (i, off) in [0xfe0u64, 0xfe4, 0xfe8, 0xfec].iter().enumerate() {
            let mut buf = [0u8; 4];
            uart.read(*off, &mut buf);
            id |= u32::from_le_bytes(buf) << (8 * i);
        }
        assert_eq!(id & 0x000f_ffff, 0x0004_1011, "amba periphid mismatch");
    }

    #[test]
    fn receive_path_tracks_fifo_emptiness() {
        let (mut uart, _) = uart();
        let mut buf = [0u8; 4];

        uart.read(UARTFR, &mut buf);
        assert_ne!(u32::from_le_bytes(buf) & FR_RXFE, 0, "should start empty");

        assert!(uart.enqueue_input(b'x'));
        uart.read(UARTFR, &mut buf);
        assert_eq!(u32::from_le_bytes(buf) & FR_RXFE, 0, "should be non-empty");

        uart.read(UARTDR, &mut buf);
        assert_eq!(u32::from_le_bytes(buf) & 0xff, u32::from(b'x'));

        uart.read(UARTFR, &mut buf);
        assert_ne!(
            u32::from_le_bytes(buf) & FR_RXFE,
            0,
            "should be empty again"
        );
    }

    #[test]
    fn input_is_bounded_and_drops_rather_than_growing() {
        let (mut uart, _) = uart();
        for _ in 0..RX_CAPACITY {
            assert!(uart.enqueue_input(b'a'));
        }
        assert!(!uart.enqueue_input(b'b'), "must refuse past capacity");
    }

    #[test]
    fn interrupt_status_masks_and_clears() {
        let (mut uart, _) = uart();
        let mut buf = [0u8; 4];

        uart.enqueue_input(b'z');
        uart.read(UARTMIS, &mut buf);
        assert_eq!(u32::from_le_bytes(buf), 0, "masked off by default");

        uart.write(UARTIMSC, &INT_RX.to_le_bytes());
        uart.read(UARTMIS, &mut buf);
        assert_eq!(u32::from_le_bytes(buf), INT_RX, "unmasked, so visible");

        uart.write(UARTICR, &INT_RX.to_le_bytes());
        uart.read(UARTRIS, &mut buf);
        assert_eq!(u32::from_le_bytes(buf) & INT_RX, 0, "write-one-to-clear");
    }
}
