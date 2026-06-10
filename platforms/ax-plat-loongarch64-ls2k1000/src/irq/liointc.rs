use core::sync::atomic::{AtomicU32, Ordering};

use ax_plat::mem::{pa, phys_to_virt};

use crate::config::devices::{LIOINTC_ISR_PADDR, LIOINTC_PADDR};

const SUPPORTED_IRQS: usize = 32;

const ROUTE_BASE: usize = 0x00;
const REG_ENABLE: usize = 0x28;
const REG_DISABLE: usize = 0x2c;
const REG_POLARITY: usize = 0x30;
const REG_EDGE: usize = 0x34;

const ROUTE_CPU0: u8 = 1 << 0;
const ROUTE_INT0: u8 = 1 << 4;

static ENABLED_IRQS: AtomicU32 = AtomicU32::new(0);

fn liointc_base() -> *mut u8 {
    phys_to_virt(pa!(LIOINTC_PADDR)).as_mut_ptr()
}

fn liointc_isr_base() -> *mut u8 {
    phys_to_virt(pa!(LIOINTC_ISR_PADDR)).as_mut_ptr()
}

fn write_reg_u32(offset: usize, value: u32) {
    unsafe {
        (liointc_base().add(offset) as *mut u32).write_volatile(value);
    }
}

fn read_isr_u32(offset: usize) -> u32 {
    unsafe { (liointc_isr_base().add(offset) as *const u32).read_volatile() }
}

fn write_route(irq: usize, value: u8) {
    unsafe {
        liointc_base().add(ROUTE_BASE + irq).write_volatile(value);
    }
}

pub(super) fn init() {
    ENABLED_IRQS.store(0, Ordering::Release);

    for irq in 0..SUPPORTED_IRQS {
        write_route(irq, ROUTE_CPU0 | ROUTE_INT0);
    }

    write_reg_u32(REG_DISABLE, u32::MAX);
    write_reg_u32(REG_EDGE, 0);
    // LIOINTC POL=0 selects active-high level interrupts.
    write_reg_u32(REG_POLARITY, 0);

    debug!(
        "LIOINTC initialized: base={:#x}, isr={:#x}, supported_irqs={}",
        LIOINTC_PADDR, LIOINTC_ISR_PADDR, SUPPORTED_IRQS
    );
}

pub(super) fn enable_irq(irq: usize) -> bool {
    if irq >= SUPPORTED_IRQS {
        return false;
    }

    let mask = 1u32 << irq;
    ENABLED_IRQS.fetch_or(mask, Ordering::AcqRel);
    write_reg_u32(REG_ENABLE, mask);
    true
}

pub(super) fn disable_irq(irq: usize) -> bool {
    if irq >= SUPPORTED_IRQS {
        return false;
    }

    let mask = 1u32 << irq;
    ENABLED_IRQS.fetch_and(!mask, Ordering::AcqRel);
    write_reg_u32(REG_DISABLE, mask);
    true
}

pub(super) fn claim_irq() -> Option<usize> {
    let pending = read_isr_u32(0) & ENABLED_IRQS.load(Ordering::Acquire);
    (pending != 0).then(|| pending.trailing_zeros() as usize)
}

pub(super) fn complete_irq(_irq: usize) {}
