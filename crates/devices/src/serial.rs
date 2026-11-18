//! ns16550 debug UART, a thin MMIO wrapper over `vm-superio`.

use std::convert::Infallible;
use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};

use vm_superio::serial::NoEvents;
use vm_superio::{Serial as Uart, Trigger};

use crate::{InterruptLine, MmioDevice, copy};

/// `vm-superio` only signals that an interrupt became pending; it never lowers the line.
/// The board therefore describes this line as edge-triggered and each signal is one rising edge.
struct RisingEdge(Arc<dyn InterruptLine>);

impl Trigger for RisingEdge {
    type E = Infallible;

    fn trigger(&self) -> Result<(), Infallible> {
        self.0.set(true);
        Ok(())
    }
}

pub struct Serial<W: Write + Send> {
    uart: Mutex<Uart<RisingEdge, NoEvents, W>>,
}

impl<W: Write + Send> Serial<W> {
    pub fn new(line: Arc<dyn InterruptLine>, out: W) -> Self {
        Self {
            uart: Mutex::new(Uart::new(RisingEdge(line), out)),
        }
    }

    /// Feeds host input to the guest. Returns how many bytes the receive FIFO took.
    pub fn enqueue(&self, bytes: &[u8]) -> usize {
        let mut uart = self.uart.lock().unwrap_or_else(PoisonError::into_inner);
        let room = uart.fifo_capacity().min(bytes.len());
        // The trigger cannot fail, and `room` never exceeds the FIFO's free space.
        uart.enqueue_raw_bytes(&bytes[..room]).unwrap_or(0)
    }
}

impl<W: Write + Send> MmioDevice for Serial<W> {
    fn read(&self, offset: u64, data: &mut [u8]) {
        // The 16550 has byte-wide registers; wider accesses see the register in the low byte.
        data.fill(0);
        if let (Ok(register), Some(first)) = (u8::try_from(offset), data.first_mut()) {
            *first = self
                .uart
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .read(register);
        }
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let (Ok(register), Some(value)) = (u8::try_from(offset), data.first()) else {
            return;
        };
        let mut uart = self.uart.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(err) = uart.write(register, *value) {
            log::warn!("{}", copy::serial_output_failed(&err));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    // 16550 register offsets and bits used by the tests.
    const REG_DATA: u64 = 0;
    const REG_INTERRUPT_ENABLE: u64 = 1;
    const REG_LINE_STATUS: u64 = 5;
    const IER_RX_AVAILABLE: u8 = 1;
    const LSR_DATA_READY: u8 = 1;

    #[derive(Default)]
    struct EdgeCounter(AtomicU32);

    impl InterruptLine for EdgeCounter {
        fn set(&self, level: bool) {
            assert!(level, "the UART line is only ever pulsed high");
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn read_reg(serial: &Serial<Sink>, reg: u64) -> u8 {
        let mut byte = [0u8];
        serial.read(reg, &mut byte);
        byte[0]
    }

    #[test]
    fn guest_output_reaches_the_host_writer() {
        let sink = Sink::default();
        let serial = Serial::new(Arc::new(EdgeCounter::default()), sink.clone());
        for byte in b"ok\n" {
            serial.write(REG_DATA, &[*byte]);
        }
        assert_eq!(*sink.0.lock().unwrap(), b"ok\n");
    }

    #[test]
    fn host_input_raises_an_interrupt_and_is_read_back_in_order() {
        let line = Arc::new(EdgeCounter::default());
        let serial = Serial::new(line.clone(), Sink::default());
        serial.write(REG_INTERRUPT_ENABLE, &[IER_RX_AVAILABLE]);
        let before = line.0.load(Ordering::SeqCst);

        assert_eq!(serial.enqueue(b"hi"), 2);
        assert!(line.0.load(Ordering::SeqCst) > before);
        assert_ne!(read_reg(&serial, REG_LINE_STATUS) & LSR_DATA_READY, 0);
        assert_eq!(read_reg(&serial, REG_DATA), b'h');
        assert_eq!(read_reg(&serial, REG_DATA), b'i');
        assert_eq!(read_reg(&serial, REG_LINE_STATUS) & LSR_DATA_READY, 0);
    }

    #[test]
    fn input_beyond_the_fifo_is_reported_as_not_taken() {
        let serial = Serial::new(Arc::new(EdgeCounter::default()), Sink::default());
        let flood = vec![b'x'; 4096];
        let taken = serial.enqueue(&flood);
        assert!(taken > 0 && taken < flood.len());
        assert_eq!(serial.enqueue(&flood), 0);
    }

    #[test]
    fn accesses_outside_the_register_block_are_ignored() {
        let serial = Serial::new(Arc::new(EdgeCounter::default()), Sink::default());
        let mut word = [0xffu8; 4];
        serial.read(0x1000, &mut word);
        assert_eq!(word, [0; 4]);
        serial.write(0x1000, &[1]);
        serial.write(REG_DATA, &[]);
    }
}
