use ax_plat::irq::{IpiTarget, IrqIf, dispatch_irq};
use loongArch64::{
    iocsr::{iocsr_read_w, iocsr_write_w},
    register::{
        ecfg::{self, LineBasedInterrupt},
        ticlr,
    },
};

use crate::config::devices::{IPI_IRQ, TIMER_IRQ};

const IOCSR_IPI_SEND_CPU_SHIFT: u32 = 16;
const IOCSR_IPI_SEND_BLOCKING: u32 = 1 << 31;

// [Loongson 3A5000 Manual](https://loongson.github.io/LoongArch-Documentation/Loongson-3A5000-usermanual-EN.html)
// See Section 10.2 for details about IPI registers
const IOCSR_IPI_STATUS: usize = 0x1000;
const IOCSR_IPI_ENABLE: usize = 0x1004;
const IOCSR_IPI_CLEAR: usize = 0x100c;
const IOCSR_IPI_SEND: usize = 0x1040;

fn make_ipi_send_value(cpu_id: usize, vector: u32, blocking: bool) -> u32 {
    let mut value = (cpu_id as u32) << IOCSR_IPI_SEND_CPU_SHIFT | vector;
    if blocking {
        value |= IOCSR_IPI_SEND_BLOCKING;
    }
    value
}

pub(crate) fn init() {
    // External LIOINTC routing is board-specific and will be wired after
    // timer/IPI bring-up is verified.
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IrqType {
    Timer,
    Ipi,
    Ex(usize),
}

impl IrqType {
    fn new(irq: usize) -> Self {
        match irq {
            TIMER_IRQ => Self::Timer,
            IPI_IRQ => Self::Ipi,
            n => Self::Ex(n),
        }
    }

    fn as_usize(&self) -> usize {
        match self {
            IrqType::Timer => TIMER_IRQ,
            IrqType::Ipi => IPI_IRQ,
            IrqType::Ex(n) => *n,
        }
    }

    fn as_line(&self) -> Option<LineBasedInterrupt> {
        match self {
            IrqType::Timer => Some(LineBasedInterrupt::TIMER),
            IrqType::Ipi => Some(LineBasedInterrupt::IPI),
            _ => None,
        }
    }
}

struct IrqIfImpl;

#[impl_plat_interface]
impl IrqIf for IrqIfImpl {
    /// Enables or disables the given IRQ.
    fn set_enable(irq: usize, enabled: bool) {
        let irq = IrqType::new(irq);

        match irq {
            IrqType::Ipi => {
                let value = if enabled { u32::MAX } else { 0 };
                iocsr_write_w(IOCSR_IPI_ENABLE, value);
            }
            IrqType::Ex(irq) => debug!("External IRQ {irq} enable={enabled} is not wired yet"),
            _ => {}
        }

        if let Some(line) = irq.as_line() {
            let old_value = ecfg::read().lie();
            let new_value = match enabled {
                true => old_value | line,
                false => old_value & !line,
            };
            ecfg::set_lie(new_value);
        }
    }

    /// Handles the IRQ.
    fn handle(irq: usize) -> Option<usize> {
        let irq = IrqType::new(irq);

        trace!("IRQ {irq:?}");

        if let IrqType::Ipi = irq {
            let mut status = iocsr_read_w(IOCSR_IPI_STATUS);
            if status != 0 {
                iocsr_write_w(IOCSR_IPI_CLEAR, status);
                trace!("IPI status = {:#x}", status);

                while status != 0 {
                    let vector = status.trailing_zeros() as usize;
                    status &= !(1 << vector);
                    if !dispatch_irq(irq.as_usize()).handled {
                        warn!("Unhandled IRQ {irq:?}");
                    }
                }
            }
        } else {
            if !dispatch_irq(irq.as_usize()).handled {
                debug!("Unhandled IRQ {irq:?}");
            }
        }

        if irq == IrqType::Timer {
            ticlr::clear_timer_interrupt();
        }

        Some(irq.as_usize())
    }

    /// Sends an inter-processor interrupt (IPI) to the specified target CPU or all CPUs.
    ///
    /// Runtime IPIs are sent NON-blocking (`IOCSR_IPI_SEND_BLOCKING` unset). The
    /// blocking variant stalls the issuing CPU until the target clears its
    /// `IOCSR_IPI_STATUS`; under a high-rate IPI burst the sender can block while
    /// the target is mid-handler (IRQs disabled) — or while the sender itself holds
    /// an IRQ-disabling lock — which deadlocks (the arceos-ipi SMP test hung 6h on
    /// loongarch). Linux/riscv/x86 likewise fire runtime IPIs non-blocking; the
    /// blocking form is reserved for the secondary-CPU boot mailbox (see `mp.rs`).
    fn send_ipi(_irq_num: usize, target: IpiTarget) {
        match target {
            IpiTarget::Current { cpu_id } => {
                iocsr_write_w(IOCSR_IPI_SEND, make_ipi_send_value(cpu_id, 0, false));
            }
            IpiTarget::Other { cpu_id } => {
                iocsr_write_w(IOCSR_IPI_SEND, make_ipi_send_value(cpu_id, 0, false));
            }
            IpiTarget::AllExceptCurrent { cpu_id, cpu_num } => {
                for i in 0..cpu_num {
                    if i != cpu_id {
                        iocsr_write_w(IOCSR_IPI_SEND, make_ipi_send_value(i, 0, false));
                    }
                }
            }
        }
    }
}
