OUTPUT_ARCH(%ARCH%)

BASE_ADDRESS = %KERNEL_BASE_VADDR%;
PHYS_VIRT_OFFSET = %PHYS_VIRT_OFFSET%;
AX_LINKER_LOAD_OFFSET = 0;
PHYS_BASE_ADDRESS = %KERNEL_BASE_PADDR%;
CPU_NUM = %CPU_NUM%;

EXTERN(_start)
/*
 * U-Boot `bootelf` jumps to the ELF entry directly, so the entry must be a
 * mapped higher-half address on LS2K1000.
 */
ENTRY(%ENTRY_VADDR%)

SECTIONS
{
    . = BASE_ADDRESS;
    _skernel = .;

    _stext = .;
    .text : AT(ADDR(.text)) ALIGN(4K) {
        /*
         * The axplat LoongArch boot stub starts with a 64-byte Linux/EFI
         * boot header. Keep `.text.boot`, but make the ELF entry point land on
         * the first real instruction after the header.
         */
        KEEP(*(.text.boot))
        _start_actual = _start + 0x40;
        *(.text .text.*)
        . = ALIGN(4K);
    }
    _etext = .;

    _srodata = .;
    .rodata : AT(ADDR(.rodata)) ALIGN(4K) {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }
    _erodata = .;

    . = ALIGN(4K);
    .data : AT(ADDR(.data)) ALIGN(4K) {
        _sdata = .;
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    /*
     * Trap handler slices emitted by `linkme` must live in the regular
     * higher-half image. If left orphaned, ld can place them at VMA 0, which
     * makes the trap dispatcher load a NULL handler pointer from the low
     * linear map and jump to 0x0 once IRQs are enabled.
     */
    . = ALIGN(8);
    .linkme : AT(ADDR(.linkme)) {
        __start_linkme_PAGE_FAULT = .;
        KEEP(*(linkme_PAGE_FAULT))
        __stop_linkme_PAGE_FAULT = .;
        __start_linkme_IRQ = .;
        KEEP(*(linkme_IRQ))
        __stop_linkme_IRQ = .;
    }

    .tdata : AT(ADDR(.tdata)) ALIGN(0x10) {
        _stdata = .;
        *(.tdata .tdata.*)
        _etdata = .;
    }

    .tbss : AT(ADDR(.tbss)) ALIGN(0x10) {
        _stbss = .;
        *(.tbss .tbss.*)
        *(.tcommon)
        _etbss = .;
    }

    . = ALIGN(64);
    /*
     * Keep `.percpu` in the higher-half image so its load address follows the
     * same physical/virtual offset as the rest of the kernel image.
     */
    .percpu : AT(ADDR(.percpu)) {
        _percpu_start = .;
        _percpu_load_start = .;
        *(.percpu .percpu.*)
        _percpu_load_end = .;
        _percpu_load_end_aligned = ALIGN(64);
        . = _percpu_start
          + (_percpu_load_end_aligned - _percpu_load_start) * CPU_NUM;
    }
    _percpu_end = .;

    . = ALIGN(4K);
    _edata = .;

    _sbss = .;
    .bss : AT(ADDR(.bss)) ALIGN(4K) {
        boot_stack = .;
        *(.bss.stack)
        . = ALIGN(4K);
        boot_stack_top = .;

        _sbss = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        *(COMMON)
        . = ALIGN(4K);
        _ebss = .;
    }

    _ekernel = .;

    /DISCARD/ : {
        *(.eh_frame)
        *(.comment)
        *(.note)
        *(.note.gnu.build-id)
    }
}
