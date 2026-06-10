#[cfg(feature = "irq")]
use ax_plat::console::ConsoleIrqEvent;
use ax_plat::{
    console::ConsoleIf,
    mem::{pa, phys_to_virt},
};

#[cfg(feature = "irq")]
use crate::config::devices::UART_IRQ;
use crate::config::devices::UART_PADDR;

const UART_RBR: usize = 0;
const UART_THR: usize = 0;
const UART_IER: usize = 1;
const UART_FCR: usize = 2;
#[cfg(feature = "irq")]
const UART_IIR: usize = 2;
const UART_LCR: usize = 3;
const UART_LSR: usize = 5;
#[cfg(feature = "irq")]
const UART_MSR: usize = 6;

const LSR_DATA_READY: u8 = 1 << 0;
const LSR_THR_EMPTY: u8 = 1 << 5;

#[cfg(feature = "irq")]
const IER_DATA_READY: u8 = 1 << 0;
#[cfg(feature = "irq")]
const IIR_NO_INT: u8 = 1 << 0;
#[cfg(feature = "irq")]
const IIR_ID_MASK: u8 = 0x0e;
#[cfg(feature = "irq")]
const IIR_MODEM_STATUS: u8 = 0x00;
#[cfg(feature = "irq")]
const IIR_THR_EMPTY: u8 = 0x02;
#[cfg(feature = "irq")]
const IIR_DATA_READY: u8 = 0x04;
#[cfg(feature = "irq")]
const IIR_LINE_STATUS: u8 = 0x06;
#[cfg(feature = "irq")]
const IIR_RX_TIMEOUT: u8 = 0x0c;

fn uart_base() -> *mut u8 {
    phys_to_virt(pa!(UART_PADDR)).as_mut_ptr()
}

fn write_byte(byte: u8) {
    let uart = uart_base();
    let mut timeout = 0x100000usize;
    unsafe {
        while timeout > 0 {
            if uart.add(UART_LSR).read_volatile() & LSR_THR_EMPTY != 0 {
                break;
            }
            timeout -= 1;
        }
        uart.add(UART_THR).write_volatile(byte);
    }
}

fn read_pending(bytes: &mut [u8]) -> usize {
    let uart = uart_base();
    let mut read = 0;
    unsafe {
        for byte in bytes {
            if uart.add(UART_LSR).read_volatile() & LSR_DATA_READY == 0 {
                break;
            }
            *byte = uart.add(UART_RBR).read_volatile();
            read += 1;
        }
    }
    read
}

#[cfg(feature = "irq")]
fn read_reg(offset: usize) -> u8 {
    unsafe { uart_base().add(offset).read_volatile() }
}

pub(crate) fn init_early() {
    let uart = uart_base();
    unsafe {
        uart.add(UART_IER).write_volatile(0);
        uart.add(UART_LCR).write_volatile(0x03);
        uart.add(UART_FCR).write_volatile(0x07);
    }
}

struct ConsoleIfImpl;

#[impl_plat_interface]
impl ConsoleIf for ConsoleIfImpl {
    /// Writes bytes to the console from input u8 slice.
    fn write_bytes(bytes: &[u8]) {
        for &byte in bytes {
            write_byte(byte);
        }
    }

    /// Reads bytes from the console into the given mutable slice.
    /// Returns the number of bytes read.
    fn read_bytes(bytes: &mut [u8]) -> usize {
        read_pending(bytes)
    }

    /// Returns the IRQ number for the console, if applicable.
    #[cfg(feature = "irq")]
    fn irq_num() -> Option<usize> {
        Some(UART_IRQ)
    }

    #[cfg(feature = "irq")]
    fn set_input_irq_enabled(enabled: bool) {
        let uart = uart_base();
        unsafe {
            uart.add(UART_IER)
                .write_volatile(if enabled { IER_DATA_READY } else { 0x00 });
        }
    }

    #[cfg(feature = "irq")]
    fn handle_irq() -> ConsoleIrqEvent {
        let iir = read_reg(UART_IIR);
        if iir & IIR_NO_INT != 0 {
            return ConsoleIrqEvent::empty();
        }

        match iir & IIR_ID_MASK {
            IIR_LINE_STATUS => {
                let _ = read_reg(UART_LSR);
                ConsoleIrqEvent::RX_ERROR
            }
            IIR_DATA_READY | IIR_RX_TIMEOUT => ConsoleIrqEvent::RX_READY,
            IIR_MODEM_STATUS => {
                let _ = read_reg(UART_MSR);
                ConsoleIrqEvent::empty()
            }
            IIR_THR_EMPTY => ConsoleIrqEvent::empty(),
            _ => ConsoleIrqEvent::empty(),
        }
    }
}
