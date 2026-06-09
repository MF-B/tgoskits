use ax_plat::{
    mem::{pa, phys_to_virt},
    power::PowerIf,
};

use crate::config::devices::{POWEROFF_MASK, POWEROFF_OFFSET, POWEROFF_VALUE, SYSCON_PADDR};

struct PowerImpl;

#[impl_plat_interface]
impl PowerIf for PowerImpl {
    /// Bootstraps the given CPU core with the given initial stack (in physical
    /// address).
    ///
    /// Where `cpu_id` is the logical CPU ID (0, 1, ..., N-1, N is the number of
    /// CPU cores on the platform).
    #[cfg(feature = "smp")]
    fn cpu_boot(cpu_id: usize, stack_top_paddr: usize) {
        crate::mp::start_secondary_cpu(cpu_id, pa!(stack_top_paddr));
    }

    /// Shutdown the whole system.
    fn system_off() -> ! {
        let poweroff_reg =
            phys_to_virt(pa!(SYSCON_PADDR + POWEROFF_OFFSET)).as_mut_ptr() as *mut u32;
        let mask = POWEROFF_MASK as u32;
        let value = POWEROFF_VALUE as u32;

        info!("Shutting down...");
        unsafe {
            let old_value = poweroff_reg.read_volatile();
            poweroff_reg.write_volatile((old_value & !mask) | (value & mask));
        }
        ax_cpu::asm::halt();
        warn!("It should shutdown!");
        loop {
            ax_cpu::asm::halt();
        }
    }

    /// Get the number of CPU cores available on this platform.
    fn cpu_num() -> usize {
        crate::config::plat::MAX_CPU_NUM
    }
}
