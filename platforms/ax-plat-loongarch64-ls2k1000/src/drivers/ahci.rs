use core::{mem::size_of, ptr::addr_of_mut};

use ax_plat::{
    mem::{pa, phys_to_virt, va, virt_to_phys},
    time::{Duration, busy_wait},
};

use crate::config::devices::{AHCI_PADDR, AHCI_PORTS_IMPLEMENTED};

const REG_CAP: usize = 0x00;
const REG_GHC: usize = 0x04;
const REG_IS: usize = 0x08;
const REG_PI: usize = 0x0c;
const REG_VS: usize = 0x10;
const REG_CAP2: usize = 0x24;
const REG_BOHC: usize = 0x28;

const HOST_CAP_SSS: u32 = 0x1 << 27;
const HOST_CAP_MPS: u32 = 0x1 << 28;
const HOST_CTL_RESET: u32 = 0x1 << 0;
const HOST_CTL_AHCI_EN: u32 = 0x1 << 31;

const PORT_BASE: usize = 0x100;
const PORT_STRIDE: usize = 0x80;
const PORT_CLB: usize = 0x00;
const PORT_CLBU: usize = 0x04;
const PORT_FB: usize = 0x08;
const PORT_FBU: usize = 0x0c;
const PORT_IS: usize = 0x10;
const PORT_IE: usize = 0x14;
const PORT_CMD: usize = 0x18;
const PORT_TFD: usize = 0x20;
const PORT_SIG: usize = 0x24;
const PORT_SSTS: usize = 0x28;
const PORT_SCTL: usize = 0x2c;
const PORT_SERR: usize = 0x30;
const PORT_CI: usize = 0x38;

const PORT_CMD_ICC_MASK: u32 = 0xf << 28;
const PORT_CMD_ICC_ACTIVE: u32 = 0x1 << 28;
const PORT_CMD_LIST_ON: u32 = 0x1 << 15;
const PORT_CMD_FIS_ON: u32 = 0x1 << 14;
const PORT_CMD_FIS_RX: u32 = 0x1 << 4;
const PORT_CMD_POWER_ON: u32 = 0x1 << 2;
const PORT_CMD_SPIN_UP: u32 = 0x1 << 1;
const PORT_CMD_START: u32 = 0x1 << 0;

const PORT_SCTL_DET_MASK: u32 = 0x0f;
const PORT_SCTL_DET_NONE: u32 = 0x0;
const PORT_SCTL_DET_INIT: u32 = 0x1;

const PORT_TFD_ERR: u32 = 0x1 << 0;
const PORT_TFD_DRQ: u32 = 0x1 << 3;
const PORT_TFD_BSY: u32 = 0x1 << 7;

const AHCI_COMRESET_ASSERT_MILLIS: u64 = 10;
const AHCI_DEVICE_READY_TIMEOUT_MILLIS: usize = 5000;
const AHCI_HBA_RESET_TIMEOUT_MILLIS: usize = 1000;
const AHCI_COMMAND_TIMEOUT_MILLIS: usize = 5000;
const AHCI_LINK_TIMEOUT_MILLIS: usize = 1000;

const AHCI_CMD_LIST_SIZE: usize = 1024;
const AHCI_RX_FIS_SIZE: usize = 256;
const AHCI_CMD_TABLE_SIZE: usize = 256;
const AHCI_IDENTIFY_SIZE: usize = 512;
const AHCI_CMD_TABLE_PRDT_OFFSET: usize = 128;

const SATA_FIS_TYPE_REGISTER_H2D: u8 = 0x27;
const SATA_FIS_H2D_COMMAND: u8 = 0x80;
const ATA_CMD_IDENTIFY_DEVICE: u8 = 0xec;
const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
const ATA_DEVICE_LBA: u8 = 0x40;
const AHCI_CMD_SLOT0: u32 = 1;

#[repr(C, align(1024))]
struct AhciDma {
    cmd_list: [u8; AHCI_CMD_LIST_SIZE],
    rx_fis: [u8; AHCI_RX_FIS_SIZE],
    cmd_table: [u8; AHCI_CMD_TABLE_SIZE],
    identify: [u8; AHCI_IDENTIFY_SIZE],
    sector: [u8; AHCI_IDENTIFY_SIZE],
}

#[repr(C)]
struct AhciCmdHeader {
    opts: u32,
    status: u32,
    tbl_addr_lo: u32,
    tbl_addr_hi: u32,
    reserved: [u32; 4],
}

#[repr(C)]
struct AhciPrdtEntry {
    addr_lo: u32,
    addr_hi: u32,
    reserved: u32,
    flags_size: u32,
}

struct DmaPtrs {
    cmd_list: *mut u8,
    rx_fis: *mut u8,
    cmd_table: *mut u8,
    identify: *mut u8,
    sector: *mut u8,
}

struct AtaDmaCommand<'a> {
    command: u8,
    data: *mut u8,
    data_len: usize,
    lba: u64,
    sectors: u16,
    device: u8,
    label: &'a str,
}

static mut AHCI_DMA: AhciDma = AhciDma {
    cmd_list: [0; AHCI_CMD_LIST_SIZE],
    rx_fis: [0; AHCI_RX_FIS_SIZE],
    cmd_table: [0; AHCI_CMD_TABLE_SIZE],
    identify: [0; AHCI_IDENTIFY_SIZE],
    sector: [0; AHCI_IDENTIFY_SIZE],
};

fn ahci_base() -> *mut u8 {
    phys_to_virt(pa!(AHCI_PADDR)).as_mut_ptr()
}

fn read_reg_u32(offset: usize) -> u32 {
    unsafe { ahci_base().add(offset).cast::<u32>().read_volatile() }
}

fn write_reg_u32(offset: usize, value: u32) {
    unsafe { ahci_base().add(offset).cast::<u32>().write_volatile(value) }
}

fn read_port_reg_u32(port: usize, offset: usize) -> u32 {
    read_reg_u32(PORT_BASE + port * PORT_STRIDE + offset)
}

fn write_port_reg_u32(port: usize, offset: usize, value: u32) {
    write_reg_u32(PORT_BASE + port * PORT_STRIDE + offset, value)
}

fn dma_barrier() {
    unsafe {
        core::arch::asm!("dbar 0");
    }
}

fn dma_ptrs() -> DmaPtrs {
    unsafe {
        let dma = addr_of_mut!(AHCI_DMA);
        DmaPtrs {
            cmd_list: addr_of_mut!((*dma).cmd_list).cast::<u8>(),
            rx_fis: addr_of_mut!((*dma).rx_fis).cast::<u8>(),
            cmd_table: addr_of_mut!((*dma).cmd_table).cast::<u8>(),
            identify: addr_of_mut!((*dma).identify).cast::<u8>(),
            sector: addr_of_mut!((*dma).sector).cast::<u8>(),
        }
    }
}

fn clear_dma() -> DmaPtrs {
    let ptrs = dma_ptrs();
    unsafe {
        (addr_of_mut!(AHCI_DMA) as *mut u8).write_bytes(0, size_of::<AhciDma>());
    }
    ptrs
}

fn dma_paddr(ptr: *const u8) -> u64 {
    virt_to_phys(va!(ptr as usize)).as_usize() as u64
}

fn write_addr_pair(port: usize, lo_offset: usize, hi_offset: usize, paddr: u64) {
    write_port_reg_u32(port, lo_offset, paddr as u32);
    write_port_reg_u32(port, hi_offset, (paddr >> 32) as u32);
}

fn port_count(cap: u32) -> usize {
    ((cap & 0x1f) + 1) as usize
}

fn ssts_det(ssts: u32) -> u32 {
    ssts & 0x0f
}

fn ssts_spd(ssts: u32) -> u32 {
    (ssts >> 4) & 0x0f
}

fn ssts_ipm(ssts: u32) -> u32 {
    (ssts >> 8) & 0x0f
}

fn wait_hba_reset_done() -> bool {
    for _ in 0..AHCI_HBA_RESET_TIMEOUT_MILLIS {
        if read_reg_u32(REG_GHC) & HOST_CTL_RESET == 0 {
            return true;
        }
        busy_wait(Duration::from_millis(1));
    }
    false
}

fn reset_hba() {
    let ghc = read_reg_u32(REG_GHC);
    info!("AHCI HBA reset: ghc={ghc:#010x}");

    write_reg_u32(REG_GHC, ghc | HOST_CTL_RESET);
    if !wait_hba_reset_done() {
        warn!("AHCI HBA reset did not complete");
        return;
    }

    let ghc = read_reg_u32(REG_GHC);
    write_reg_u32(REG_GHC, ghc | HOST_CTL_AHCI_EN);
    busy_wait(Duration::from_millis(1));

    info!("AHCI HBA reset done: ghc={:#010x}", read_reg_u32(REG_GHC));
}

fn configure_hba_cap() {
    let cap = read_reg_u32(REG_CAP);
    let new_cap = cap | HOST_CAP_MPS | HOST_CAP_SSS;
    if new_cap == cap {
        return;
    }

    info!("AHCI CAP update: {cap:#010x} -> {new_cap:#010x}");
    write_reg_u32(REG_CAP, new_cap);
    info!("AHCI CAP after update: {:#010x}", read_reg_u32(REG_CAP));
}

fn log_port(port: usize, stage: &str) {
    let cmd = read_port_reg_u32(port, PORT_CMD);
    let tfd = read_port_reg_u32(port, PORT_TFD);
    let sig = read_port_reg_u32(port, PORT_SIG);
    let ssts = read_port_reg_u32(port, PORT_SSTS);
    let sctl = read_port_reg_u32(port, PORT_SCTL);
    let serr = read_port_reg_u32(port, PORT_SERR);

    info!(
        "AHCI port{port} {stage}: cmd={cmd:#010x}, tfd={tfd:#010x}, sig={sig:#010x}, \
         ssts={ssts:#010x}, sctl={sctl:#010x}, serr={serr:#010x}, det={}, spd={}, ipm={}",
        ssts_det(ssts),
        ssts_spd(ssts),
        ssts_ipm(ssts),
    );
}

fn power_up_port(port: usize) {
    let cmd = read_port_reg_u32(port, PORT_CMD);
    if cmd & (PORT_CMD_LIST_ON | PORT_CMD_FIS_ON | PORT_CMD_FIS_RX | PORT_CMD_START) != 0 {
        warn!("AHCI port{port} command engine is already active: cmd={cmd:#010x}");
    }

    let new_cmd =
        (cmd & !PORT_CMD_ICC_MASK) | PORT_CMD_ICC_ACTIVE | PORT_CMD_POWER_ON | PORT_CMD_SPIN_UP;
    write_port_reg_u32(port, PORT_CMD, new_cmd);
}

fn wait_port_link(port: usize, stage: &str) -> bool {
    for _ in 0..AHCI_LINK_TIMEOUT_MILLIS {
        let ssts = read_port_reg_u32(port, PORT_SSTS);
        if ssts_det(ssts) == 0x3 {
            info!(
                "AHCI port{port} link up after {stage}: ssts={ssts:#010x}, spd={}, ipm={}",
                ssts_spd(ssts),
                ssts_ipm(ssts),
            );
            return true;
        }
        busy_wait(Duration::from_millis(1));
    }

    let ssts = read_port_reg_u32(port, PORT_SSTS);
    warn!(
        "AHCI port{port} link not up after {stage} and {AHCI_LINK_TIMEOUT_MILLIS}ms: \
         ssts={ssts:#010x}, det={}, spd={}, ipm={}",
        ssts_det(ssts),
        ssts_spd(ssts),
        ssts_ipm(ssts),
    );
    false
}

fn clear_port_errors(port: usize, stage: &str) {
    let serr = read_port_reg_u32(port, PORT_SERR);
    if serr == 0 {
        return;
    }

    write_port_reg_u32(port, PORT_SERR, serr);
    info!(
        "AHCI port{port} clear SERR after {stage}: {serr:#010x} -> {:#010x}",
        read_port_reg_u32(port, PORT_SERR),
    );
}

fn wait_port_ready(port: usize) -> bool {
    for _ in 0..AHCI_DEVICE_READY_TIMEOUT_MILLIS {
        let tfd = read_port_reg_u32(port, PORT_TFD);
        if tfd & (PORT_TFD_BSY | PORT_TFD_DRQ) == 0 {
            info!("AHCI port{port} device ready: tfd={tfd:#010x}");
            return true;
        }
        busy_wait(Duration::from_millis(1));
    }

    let tfd = read_port_reg_u32(port, PORT_TFD);
    warn!(
        "AHCI port{port} device not ready after {AHCI_DEVICE_READY_TIMEOUT_MILLIS}ms: \
         tfd={tfd:#010x}"
    );
    false
}

fn start_command_engine(port: usize, ptrs: &DmaPtrs) -> bool {
    let cmd_list_paddr = dma_paddr(ptrs.cmd_list);
    let rx_fis_paddr = dma_paddr(ptrs.rx_fis);

    write_addr_pair(port, PORT_CLB, PORT_CLBU, cmd_list_paddr);
    write_addr_pair(port, PORT_FB, PORT_FBU, rx_fis_paddr);
    write_port_reg_u32(port, PORT_IE, 0);
    write_port_reg_u32(port, PORT_IS, u32::MAX);
    write_reg_u32(REG_IS, 1u32 << port);

    let cmd = read_port_reg_u32(port, PORT_CMD);
    let new_cmd = (cmd & !PORT_CMD_ICC_MASK)
        | PORT_CMD_ICC_ACTIVE
        | PORT_CMD_FIS_RX
        | PORT_CMD_POWER_ON
        | PORT_CMD_SPIN_UP
        | PORT_CMD_START;
    write_port_reg_u32(port, PORT_CMD, new_cmd);
    dma_barrier();

    info!(
        "AHCI port{port} command engine started: clb={cmd_list_paddr:#x}, fb={rx_fis_paddr:#x}, \
         cmd={:#010x}",
        read_port_reg_u32(port, PORT_CMD),
    );

    wait_port_ready(port)
}

fn setup_ata_dma_command(port: usize, ptrs: &DmaPtrs, command: AtaDmaCommand<'_>) {
    let cmd_table_paddr = dma_paddr(ptrs.cmd_table);
    let data_paddr = dma_paddr(command.data);

    unsafe {
        ptrs.cmd_table.write_bytes(0, AHCI_CMD_TABLE_SIZE);
        command.data.write_bytes(0, command.data_len);

        let cfis = ptrs.cmd_table;
        cfis.add(0).write(SATA_FIS_TYPE_REGISTER_H2D);
        cfis.add(1).write(SATA_FIS_H2D_COMMAND);
        cfis.add(2).write(command.command);
        cfis.add(4).write(command.lba as u8);
        cfis.add(5).write((command.lba >> 8) as u8);
        cfis.add(6).write((command.lba >> 16) as u8);
        cfis.add(7).write(command.device);
        cfis.add(8).write((command.lba >> 24) as u8);
        cfis.add(9).write((command.lba >> 32) as u8);
        cfis.add(10).write((command.lba >> 40) as u8);
        cfis.add(12).write(command.sectors as u8);
        cfis.add(13).write((command.sectors >> 8) as u8);

        ptrs.cmd_list.cast::<AhciCmdHeader>().write(AhciCmdHeader {
            opts: ((size_of::<[u8; 20]>() / 4) as u32) | (1 << 16),
            status: 0,
            tbl_addr_lo: cmd_table_paddr as u32,
            tbl_addr_hi: (cmd_table_paddr >> 32) as u32,
            reserved: [0; 4],
        });

        ptrs.cmd_table
            .add(AHCI_CMD_TABLE_PRDT_OFFSET)
            .cast::<AhciPrdtEntry>()
            .write(AhciPrdtEntry {
                addr_lo: data_paddr as u32,
                addr_hi: (data_paddr >> 32) as u32,
                reserved: 0,
                flags_size: (command.data_len as u32 - 1) | (1 << 31),
            });
    }

    write_port_reg_u32(port, PORT_IS, u32::MAX);
    write_reg_u32(REG_IS, 1u32 << port);
    dma_barrier();

    let label = command.label;
    info!("AHCI port{port} {label} setup: ctba={cmd_table_paddr:#x}, buf={data_paddr:#x}");
}

fn setup_identify_command(port: usize, ptrs: &DmaPtrs) {
    setup_ata_dma_command(
        port,
        ptrs,
        AtaDmaCommand {
            command: ATA_CMD_IDENTIFY_DEVICE,
            data: ptrs.identify,
            data_len: AHCI_IDENTIFY_SIZE,
            lba: 0,
            sectors: 0,
            device: 0,
            label: "IDENTIFY",
        },
    );
}

fn wait_command_done(port: usize) -> bool {
    for _ in 0..AHCI_COMMAND_TIMEOUT_MILLIS {
        if read_port_reg_u32(port, PORT_CI) & AHCI_CMD_SLOT0 == 0 {
            dma_barrier();
            let is = read_port_reg_u32(port, PORT_IS);
            let tfd = read_port_reg_u32(port, PORT_TFD);
            info!("AHCI port{port} command done: is={is:#010x}, tfd={tfd:#010x}");
            return tfd & (PORT_TFD_BSY | PORT_TFD_DRQ | PORT_TFD_ERR) == 0;
        }
        busy_wait(Duration::from_millis(1));
    }

    warn!(
        "AHCI port{port} command timeout: ci={:#010x}, is={:#010x}, tfd={:#010x}",
        read_port_reg_u32(port, PORT_CI),
        read_port_reg_u32(port, PORT_IS),
        read_port_reg_u32(port, PORT_TFD),
    );
    false
}

fn read_identify_word(ptrs: &DmaPtrs, word: usize) -> u16 {
    unsafe { ptrs.identify.cast::<u16>().add(word).read_volatile() }
}

fn read_identify_string<const N: usize>(ptrs: &DmaPtrs, first_word: usize) -> [u8; N] {
    let mut out = [0; N];
    for i in 0..N / 2 {
        let word = read_identify_word(ptrs, first_word + i);
        out[i * 2] = (word >> 8) as u8;
        out[i * 2 + 1] = word as u8;
    }
    out
}

fn log_identify_data(ptrs: &DmaPtrs) {
    let model = read_identify_string::<40>(ptrs, 27);
    let serial = read_identify_string::<20>(ptrs, 10);
    let model = core::str::from_utf8(&model).unwrap_or("<invalid>");
    let serial = core::str::from_utf8(&serial).unwrap_or("<invalid>");
    let lba28 = read_identify_word(ptrs, 60) as u32 | ((read_identify_word(ptrs, 61) as u32) << 16);
    let lba48 = read_identify_word(ptrs, 100) as u64
        | ((read_identify_word(ptrs, 101) as u64) << 16)
        | ((read_identify_word(ptrs, 102) as u64) << 32)
        | ((read_identify_word(ptrs, 103) as u64) << 48);

    info!(
        "AHCI IDENTIFY: model='{model}', serial='{serial}', lba28={lba28}, lba48={lba48}, \
         word0={:#06x}, word83={:#06x}",
        read_identify_word(ptrs, 0),
        read_identify_word(ptrs, 83),
    );
}

fn identify_device(port: usize, ptrs: &DmaPtrs) {
    setup_identify_command(port, ptrs);
    write_port_reg_u32(port, PORT_CI, AHCI_CMD_SLOT0);

    if wait_command_done(port) {
        log_identify_data(ptrs);
    }
}

fn log_lba0(ptrs: &DmaPtrs) {
    let b = ptrs.sector;
    let sig =
        unsafe { (b.add(511).read_volatile() as u16) << 8 | b.add(510).read_volatile() as u16 };

    info!(
        "AHCI LBA0: first16={:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} \
         {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}, sig={sig:#06x}",
        unsafe { b.add(0).read_volatile() },
        unsafe { b.add(1).read_volatile() },
        unsafe { b.add(2).read_volatile() },
        unsafe { b.add(3).read_volatile() },
        unsafe { b.add(4).read_volatile() },
        unsafe { b.add(5).read_volatile() },
        unsafe { b.add(6).read_volatile() },
        unsafe { b.add(7).read_volatile() },
        unsafe { b.add(8).read_volatile() },
        unsafe { b.add(9).read_volatile() },
        unsafe { b.add(10).read_volatile() },
        unsafe { b.add(11).read_volatile() },
        unsafe { b.add(12).read_volatile() },
        unsafe { b.add(13).read_volatile() },
        unsafe { b.add(14).read_volatile() },
        unsafe { b.add(15).read_volatile() },
    );
}

fn read_lba0(port: usize, ptrs: &DmaPtrs) {
    setup_ata_dma_command(
        port,
        ptrs,
        AtaDmaCommand {
            command: ATA_CMD_READ_DMA_EXT,
            data: ptrs.sector,
            data_len: AHCI_IDENTIFY_SIZE,
            lba: 0,
            sectors: 1,
            device: ATA_DEVICE_LBA,
            label: "READ LBA0",
        },
    );
    write_port_reg_u32(port, PORT_CI, AHCI_CMD_SLOT0);

    if wait_command_done(port) {
        log_lba0(ptrs);
    }
}

fn comreset_port(port: usize) {
    let serr = read_port_reg_u32(port, PORT_SERR);
    if serr != 0 {
        write_port_reg_u32(port, PORT_SERR, serr);
    }

    let sctl = read_port_reg_u32(port, PORT_SCTL);
    info!("AHCI port{port} COMRESET: sctl={sctl:#010x}, serr={serr:#010x}");

    write_port_reg_u32(
        port,
        PORT_SCTL,
        (sctl & !PORT_SCTL_DET_MASK) | PORT_SCTL_DET_INIT,
    );
    busy_wait(Duration::from_millis(AHCI_COMRESET_ASSERT_MILLIS));
    write_port_reg_u32(
        port,
        PORT_SCTL,
        (sctl & !PORT_SCTL_DET_MASK) | PORT_SCTL_DET_NONE,
    );
}

fn bring_up_port_link(port: usize) -> bool {
    power_up_port(port);
    if wait_port_link(port, "power-up") {
        return true;
    }

    comreset_port(port);
    power_up_port(port);
    wait_port_link(port, "COMRESET")
}

fn effective_port_map(raw_pi: u32) -> u32 {
    let board_pi = AHCI_PORTS_IMPLEMENTED as u32;
    if raw_pi != 0 || board_pi == 0 {
        return raw_pi;
    }

    info!("AHCI PI is zero; applying board ports-implemented={board_pi:#010x}");
    write_reg_u32(REG_PI, board_pi);

    let new_pi = read_reg_u32(REG_PI);
    if new_pi == 0 {
        warn!("AHCI PI write did not latch; probing board port map in software");
        board_pi
    } else {
        info!("AHCI PI after board port map write: {new_pi:#010x}");
        new_pi
    }
}

pub(super) fn probe() {
    reset_hba();
    configure_hba_cap();

    let cap = read_reg_u32(REG_CAP);
    let ghc = read_reg_u32(REG_GHC);
    let is = read_reg_u32(REG_IS);
    let raw_pi = read_reg_u32(REG_PI);
    let pi = effective_port_map(raw_pi);
    let vs = read_reg_u32(REG_VS);
    let cap2 = read_reg_u32(REG_CAP2);
    let bohc = read_reg_u32(REG_BOHC);

    info!(
        "AHCI HBA: base={AHCI_PADDR:#x}, cap={cap:#010x}, ghc={ghc:#010x}, is={is:#010x}, \
         pi={raw_pi:#010x}, effective_pi={pi:#010x}, vs={vs:#010x}, cap2={cap2:#010x}, \
         bohc={bohc:#010x}"
    );

    if cap == 0 && ghc == 0 && is == 0 && raw_pi == 0 && vs == 0 {
        warn!("AHCI HBA registers read as zero; check AHCI clock/reset and MMIO base");
        return;
    }
    if cap == u32::MAX && ghc == u32::MAX && is == u32::MAX && raw_pi == u32::MAX && vs == u32::MAX
    {
        warn!("AHCI HBA registers read as all ones; check AHCI MMIO mapping");
        return;
    }
    if pi == 0 {
        warn!("AHCI HBA reports no implemented ports");
        return;
    }

    for port in 0..port_count(cap).min(32) {
        if pi & (1u32 << port) != 0 {
            log_port(port, "before-link");
            if bring_up_port_link(port) {
                clear_port_errors(port, "link-up");
                let ptrs = clear_dma();
                if start_command_engine(port, &ptrs) {
                    identify_device(port, &ptrs);
                    read_lba0(port, &ptrs);
                }
            }
            log_port(port, "after-link");
        }
    }
}
